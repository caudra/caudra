mod layout;
mod render;
mod segment;
mod selection;
#[cfg(test)]
mod tests;
mod thinking;

use self::render::{EXPAND_AFFORDANCE, HoverFeedback, Placement, RenderCursor, RenderFeedback};
use self::segment::{Segment, SegmentCache, wrapped_line_count};
use self::thinking::ThinkingWindow;
use layout::{SegmentChrome, SegmentKind};

use super::tool_display::{
    RenderCtx, ScrollTail, ToolLines, append_annotation, append_right_info, assistant_style,
    build_instructions_lines, build_tool_lines, done_style, draws_live_script, error_style,
    format_timestamp_now, names_tool, notice_style, shell_clock_ticks, thinking_style,
    truncate_to_header, user_style,
};
use super::{
    DisplayMessage, DisplayRole, DisplaySource, ToolProgress, ToolRole, ToolStatus,
    apply_scroll_rows, automation_inspector,
    code_view::{
        BatchLiveMap, BatchProgressMap, BatchStartedMap, BatchViewMap, CardPolicy, Disclosure,
        RowTarget, ScrollSpan, ScrollWindow,
    },
    command_text, memory_card, peer_card, review, task_card, workflow_card,
    workflow_card::CardHit,
};
use crate::animation::{live_elapsed, spinner_str};
use crate::chat::batch_child_id;
use crate::components::commit_popup::CommitIndex;
use crate::components::keybindings::key;
use crate::components::prompt_progress::{self, PromptProgress, PromptRate};
use crate::highlight;
use crate::markdown::{
    DiagramSpan, LinkMap, TerminalLink, hr_line, plain_lines, text_to_painted, text_to_rows,
    truncate_output, truncate_output_tail,
};
use crate::provenance::Provenance;
use crate::render_worker::RenderWorker;
use crate::selection::Selection;
use crate::splash::{ColorTransition, Splash};
use crate::theme;
use crate::update;
use caudra_agent::types::WorkflowRunCard;
use caudra_config::{ClockFormat, ToolOutputLines, UiConfig};
use caudra_grab::grab_leaf;
use caudra_markdown::render::SpanSource;
use caudra_providers::AutomationEventOrigin;
use caudra_workflow::RunSnapshot;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem::{self, Discriminant};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::MouseEvent;

use super::scrollbar::{ScrollHint, Scrollbar, ScrollbarMouse};
use super::streaming_content::StreamingContent;
use caudra_agent::background::BackgroundTasks;
use caudra_agent::commits::{self, CommitRef};
use caudra_agent::mentions::{self, Mention};
use caudra_agent::tools::{BATCH_TOOL_NAME, SHELL_TOOL_NAME, ToolEffect};
use caudra_agent::{
    BatchToolEntry, BatchToolStatus, BufferSnapshot, CallStage, EventSender, InstructionBlock,
    NO_FILES_FOUND, ReasoningSummary, SharedBuf, SubagentProgress, TaskCard, ToolDoneEvent,
    ToolOutput, ToolStartEvent, format_live_duration, format_settled_duration, reasoning_summary,
    streaming_reasoning_summary,
};
use caudra_lua::{EventHandle, WARM_TOOL_CAP, WinView};
use caudra_storage::background::JobKind;
use caudra_storage::view::ViewMode;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use crate::repaint::{Cadence, Dirty};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use tracing::warn;

const THOUGHT_PREFIX: &str = "Thought";
const INJECTED_FALLBACK_TITLE: &str = "injected message";
const INJECTED_SEARCH_PREFIX: &str = "injected> ";
const THINKING_SEARCH_PREFIX: &str = "thinking> ";
/// Separates a batch child's roster index from its parent's tool id. Not a
/// character a tool id carries, so a child's key can never collide with a
/// card's.
const CHILD_SCROLL_INFIX: &str = "#";
const REFLOW_MARGIN_VIEWPORTS: u32 = 1;
const SHELL_LIVE_OUTPUT_LINES: usize = 12;
const RAW_HTML_CLOSINGS: [&str; 4] = ["</script>", "</pre>", "</style>", "</textarea>"];
const COMMONMARK_BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];
/// What a copied fence is called when the fragment inside it is not one
/// language's source: a transcript excerpt is text, not code.
const PLAIN_FENCE_LANGUAGE: &str = "text";
const MATH_FENCE: &str = "$$";
const MATH_BRACKET_CLOSE: &str = "\\]";
const MATH_BRACKET_OPEN: &str = "\\[";

#[derive(Debug, PartialEq, Eq)]
enum HoverTarget {
    Link(Arc<str>),
    Mention(Mention),
    Commit(CommitRef),
    MessageAction(usize),
    CachedThinking(usize),
    StreamingThinking,
    Tool { id: String, feedback: HoverFeedback },
    Diagram(DiagramKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MessageActionTarget {
    source: DisplaySource,
    segment_index: usize,
}

impl MessageActionTarget {
    pub(crate) fn source(self) -> DisplaySource {
        self.source
    }
}

#[derive(Clone, Copy)]
struct MessageActionHit {
    area: Rect,
    target: MessageActionTarget,
}

/// Identifies one drawn diagram across rebuilds. The message index is the
/// same backlink segments already carry, and `id` counts diagrams within
/// that message, so both survive a reflow.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DiagramKey {
    msg_index: usize,
    id: u16,
}

/// A rendered segment handed to the review modal. `provenance` is absent for
/// segments with no markdown behind them, such as tool buffers.
pub(crate) struct ReviewTarget {
    pub lines: Vec<Line<'static>>,
    pub provenance: Option<Provenance>,
    pub label: &'static str,
}

/// The default review surface. Notes carrying it compile without a surface
/// attribute, so the constant is shared with the review modal.
pub(crate) const ASSISTANT_LABEL: &str = "assistant reply";

fn review_label(source: DisplaySource) -> &'static str {
    match source {
        DisplaySource::User(_) => "your message",
        DisplaySource::AssistantText(_) => ASSISTANT_LABEL,
        DisplaySource::Reasoning(_) => "thinking",
        DisplaySource::ToolCall { .. } => "tool call",
        DisplaySource::ToolResult(_) => "tool result",
    }
}

fn collect_task_identities(output: &ToolOutput, identities: &mut HashMap<String, String>) {
    match output {
        ToolOutput::Tasks(tasks) => {
            identities.extend(
                tasks
                    .iter()
                    .map(|task| (task.invocation_id.clone(), task.task_id.clone())),
            );
        }
        ToolOutput::Batch { entries, .. } => {
            for entry in entries {
                if let Some(output) = &entry.output {
                    collect_task_identities(output, identities);
                }
            }
        }
        _ => {}
    }
}

fn markdown_inline(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '\n' | '\r') {
            if !escaped.ends_with(' ') {
                escaped.push(' ');
            }
            continue;
        }
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' | '&'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn markdown_code_span(text: &str) -> String {
    if text.contains(['\n', '\r']) {
        let mut single_line = String::with_capacity(text.len());
        for character in text.chars() {
            if matches!(character, '\n' | '\r') {
                if !single_line.ends_with(' ') {
                    single_line.push(' ');
                }
            } else {
                single_line.push(character);
            }
        }
        return markdown_code_span(&single_line);
    }
    if text.is_empty()
        || text.starts_with('`')
        || text.starts_with(' ')
        || text.ends_with('`')
        || text.ends_with(' ')
    {
        return format!("<code>{}</code>", html_text(text));
    }
    let longest_run = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let delimiter = "`".repeat(longest_run.saturating_add(1).max(1));
    format!("{delimiter}{text}{delimiter}")
}

fn html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn raw_html_closing(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    if line.starts_with("<!--") {
        return Some("-->");
    }
    if line.starts_with("<?") {
        return Some("?>");
    }
    if line.starts_with("<![CDATA[") {
        return Some("]]>");
    }
    if line
        .strip_prefix("<!")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|character| character.is_ascii_uppercase())
    {
        return Some(">");
    }
    for (opening, closing) in [
        ("<script", "</script>"),
        ("<pre", "</pre>"),
        ("<style", "</style>"),
        ("<textarea", "</textarea>"),
    ] {
        let Some(rest) = lower.strip_prefix(opening) else {
            continue;
        };
        if rest.is_empty() || rest.starts_with([' ', '\t', '>']) {
            return Some(closing);
        }
    }
    None
}

fn skip_html_whitespace(bytes: &[u8], mut offset: usize) -> usize {
    while bytes
        .get(offset)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        offset += 1;
    }
    offset
}

fn has_complete_html_tag_suffix(suffix: &str, closing: bool) -> bool {
    let bytes = suffix.as_bytes();
    if closing {
        let offset = skip_html_whitespace(bytes, 0);
        return bytes.get(offset) == Some(&b'>') && offset + 1 == bytes.len();
    }

    let mut offset = 0;
    loop {
        if bytes.get(offset) == Some(&b'>') {
            return offset + 1 == bytes.len();
        }
        if bytes.get(offset..offset + 2) == Some(b"/>") {
            return offset + 2 == bytes.len();
        }

        let separator_start = offset;
        offset = skip_html_whitespace(bytes, offset);
        if bytes.get(offset) == Some(&b'>') {
            return offset + 1 == bytes.len();
        }
        if bytes.get(offset..offset + 2) == Some(b"/>") {
            return offset + 2 == bytes.len();
        }
        if offset == separator_start {
            return false;
        }
        if !bytes
            .get(offset)
            .is_some_and(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b':'))
        {
            return false;
        }
        offset += 1;
        while bytes.get(offset).is_some_and(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-')
        }) {
            offset += 1;
        }

        let equals = skip_html_whitespace(bytes, offset);
        if bytes.get(equals) != Some(&b'=') {
            continue;
        }
        offset = skip_html_whitespace(bytes, equals + 1);
        match bytes.get(offset).copied() {
            Some(quote @ (b'\'' | b'"')) => {
                offset += 1;
                let Some(end) = bytes[offset..].iter().position(|byte| *byte == quote) else {
                    return false;
                };
                offset += end + 1;
            }
            Some(_) => {
                let start = offset;
                while bytes.get(offset).is_some_and(|byte| {
                    !byte.is_ascii_whitespace()
                        && !matches!(byte, b'\'' | b'"' | b'=' | b'<' | b'>' | b'`')
                }) {
                    offset += 1;
                }
                if offset == start {
                    return false;
                }
            }
            None => return false,
        }
    }
}

fn starts_blank_line_html_block(line: &str, paragraph_open: bool) -> bool {
    let line = line.trim_end_matches([' ', '\t']);
    let lower = line.to_ascii_lowercase();
    let (closing, rest) = lower
        .strip_prefix("</")
        .map(|rest| (true, rest))
        .or_else(|| lower.strip_prefix('<').map(|rest| (false, rest)))
        .unwrap_or((false, ""));
    let name_end = rest
        .find(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .unwrap_or(rest.len());
    let Some(name) = rest
        .get(..name_end)
        .filter(|name| name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic))
    else {
        return false;
    };
    let suffix = &rest[name_end..];
    let tag_boundary =
        suffix.is_empty() || suffix.starts_with([' ', '\t', '>']) || suffix.starts_with("/>");
    if tag_boundary && COMMONMARK_BLOCK_TAGS.contains(&name) {
        return true;
    }
    !paragraph_open && has_complete_html_tag_suffix(suffix, closing)
}

fn math_block_closing(line: &str) -> Option<&'static str> {
    let trimmed = line.trim();
    for (opening, closing) in [
        (MATH_FENCE, MATH_FENCE),
        (MATH_BRACKET_OPEN, MATH_BRACKET_CLOSE),
    ] {
        let Some(rest) = trimmed.strip_prefix(opening) else {
            continue;
        };
        if rest.is_empty() {
            return Some(closing);
        }
        if rest
            .strip_suffix(closing)
            .is_some_and(|inner| !inner.trim().is_empty())
        {
            return None;
        }
    }
    None
}

fn starts_commonmark_leaf_block(line: &str, paragraph_open: bool) -> bool {
    let bytes = line.as_bytes();
    let marker_run = bytes.iter().take_while(|byte| **byte == b'#').count();
    if (1..=6).contains(&marker_run)
        && bytes
            .get(marker_run)
            .is_none_or(|byte| matches!(byte, b' ' | b'\t'))
    {
        return true;
    }
    if bytes.first() == Some(&b'>')
        || bytes.get(1).is_some_and(|byte| {
            matches!(bytes[0], b'-' | b'+' | b'*') && matches!(byte, b' ' | b'\t')
        })
    {
        return true;
    }
    let ordered_marker = bytes
        .iter()
        .take(9)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if ordered_marker > 0
        && bytes
            .get(ordered_marker)
            .is_some_and(|byte| matches!(byte, b'.' | b')'))
        && bytes
            .get(ordered_marker + 1)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        return true;
    }

    let compact = line
        .bytes()
        .filter(|byte| !matches!(byte, b' ' | b'\t'))
        .collect::<Vec<_>>();
    if compact.len() >= 3
        && matches!(compact[0], b'-' | b'_' | b'*')
        && compact.iter().all(|byte| *byte == compact[0])
    {
        return true;
    }
    paragraph_open
        && !compact.is_empty()
        && matches!(compact[0], b'-' | b'=')
        && compact.iter().all(|byte| *byte == compact[0])
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MarkdownBlock {
    Fence(char, usize),
    Html(&'static str),
    HtmlBlankLine,
    Math(&'static str),
}

fn closes_html_block(line: &str, closing: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    if RAW_HTML_CLOSINGS.contains(&closing) {
        RAW_HTML_CLOSINGS
            .iter()
            .any(|candidate| lower.contains(candidate))
    } else {
        lower.contains(closing)
    }
}

fn is_fence_closing_suffix(text: &str) -> bool {
    text.chars()
        .all(|character| matches!(character, ' ' | '\t'))
}

fn unclosed_markdown_block(text: &str) -> Option<MarkdownBlock> {
    let mut open = None;
    let mut paragraph_open = false;
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for line in normalized.split('\n') {
        if let Some(MarkdownBlock::Html(closing)) = open {
            if closes_html_block(line, closing) {
                open = None;
            }
            continue;
        }
        if open.is_some_and(|block| matches!(block, MarkdownBlock::HtmlBlankLine)) {
            if line.trim().is_empty() {
                open = None;
                paragraph_open = false;
            }
            continue;
        }
        let trimmed = line.trim_start_matches(' ');
        if trimmed.trim().is_empty() {
            paragraph_open = false;
            continue;
        }
        if line.len() - trimmed.len() > 3 {
            continue;
        }
        if let Some(MarkdownBlock::Fence(marker, run)) = open {
            let marker_run = trimmed
                .chars()
                .take_while(|character| *character == marker)
                .count();
            if marker_run >= run && is_fence_closing_suffix(&trimmed[marker_run..]) {
                open = None;
            }
            continue;
        }
        if let Some(marker) = trimmed
            .chars()
            .next()
            .filter(|marker| matches!(marker, '`' | '~'))
        {
            let run = trimmed
                .chars()
                .take_while(|character| *character == marker)
                .count();
            if run >= 3 && (marker != '`' || !trimmed[run..].contains('`')) {
                open = Some(MarkdownBlock::Fence(marker, run));
                paragraph_open = false;
                continue;
            }
        }
        if let Some(closing) = raw_html_closing(trimmed) {
            if !closes_html_block(trimmed, closing) {
                open = Some(MarkdownBlock::Html(closing));
            }
            paragraph_open = false;
            continue;
        }
        if starts_blank_line_html_block(trimmed, paragraph_open) {
            open = Some(MarkdownBlock::HtmlBlankLine);
            paragraph_open = false;
            continue;
        }
        paragraph_open = !starts_commonmark_leaf_block(trimmed, paragraph_open);
    }
    open.filter(|block| !matches!(block, MarkdownBlock::HtmlBlankLine))
}

fn unclosed_caudra_fenced_block(text: &str) -> Option<MarkdownBlock> {
    let mut open = None;
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        match open {
            Some(MarkdownBlock::Fence('`', run)) => {
                let trimmed = line.trim_end();
                if trimmed.starts_with(&"`".repeat(run))
                    && trimmed.as_bytes().get(run) != Some(&b'`')
                {
                    open = None;
                }
                continue;
            }
            Some(MarkdownBlock::Math(closing)) => {
                if line.trim() == closing {
                    open = None;
                }
                continue;
            }
            _ => {}
        }

        let run = line.bytes().take_while(|byte| *byte == b'`').count();
        if run >= 3 && lines.peek().is_some() && !line[run..].contains('`') {
            open = Some(MarkdownBlock::Fence('`', run));
            continue;
        }
        if let Some(closing) = math_block_closing(line) {
            open = Some(MarkdownBlock::Math(closing));
        }
    }
    open
}

fn fenced_text(text: &str, language: Option<&str>) -> String {
    let longest_run = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let delimiter = "`".repeat(longest_run.saturating_add(1).max(3));
    let newline = if text.ends_with('\n') { "" } else { "\n" };
    let language = language.unwrap_or(PLAIN_FENCE_LANGUAGE);
    format!("{delimiter}{language}\n{text}{newline}{delimiter}")
}

fn rendered_markdown_text(text: &str, width: u16) -> String {
    let (painted, _) = text_to_painted(
        text,
        "",
        Style::default(),
        Style::default(),
        width,
        None,
        Vec::new(),
    );
    painted
        .lines
        .iter()
        .zip(&painted.provenance)
        .map(|(line, provenance)| {
            let first = provenance
                .spans
                .iter()
                .position(|source| !matches!(source, SpanSource::Chrome));
            let last = provenance
                .spans
                .iter()
                .rposition(|source| !matches!(source, SpanSource::Chrome));
            match (first, last) {
                (Some(first), Some(last)) => line.spans[first..=last]
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect(),
                _ => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::InProgress => "in progress",
        ToolStatus::Success => "success",
        ToolStatus::Error => "error",
    }
}

/// What the reader asked of one card, overriding whatever the view mode would
/// have drawn. There is no entry for the budgeted middle: that is where a card
/// rests, never where a click lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CardState {
    Closed,
    Full,
}

/// What one batch child's window is filed under. A child has no tool id of
/// its own, so it borrows its parent's and adds the roster index, the way an
/// instruction segment borrows its parent's with a suffix.
fn child_scroll_id(parent_id: &str, index: usize) -> String {
    format!("{parent_id}{CHILD_SCROLL_INFIX}{index}")
}

/// The parent and index a child's scroll key names, if it is shaped like one.
/// Only a shape: the caller confirms the child exists before taking it.
fn split_child_scroll_id(key: &str) -> Option<(&str, usize)> {
    let (parent, index) = key.rsplit_once(CHILD_SCROLL_INFIX)?;
    Some((parent, index.parse().ok()?))
}

/// One window as it was last drawn: the rows it took, and where it sits in
/// the body it is a view onto.
///
/// Collected during the segment loop and used after it, both because painting
/// a bar needs a mutable borrow the loop is holding, and because the wheel and
/// the pointer are answered from the same geometry the paint used.
struct CardWindow {
    key: String,
    body: Rect,
    total: u32,
    position: u32,
}

impl CardWindow {
    /// The bar's column: the last of the body.
    ///
    /// One column rather than the whole body, unlike the transcript's own bar,
    /// because a card body is a click target in its own right and handing the
    /// pointer to a bar would cost the reader the rest of the card.
    fn strip(&self) -> Rect {
        Rect::new(
            self.body.x + self.body.width - 1,
            self.body.y,
            1,
            self.body.height,
        )
    }
}

/// Whether the row `rel` rows into `seg` is the footer that re-pins its window.
fn on_scroll_footer(seg: &Segment, rel: u16, width: u16) -> bool {
    seg.scroll_footer_line
        .is_some_and(|line| seg.source_line_at(rel, width) == Some(line))
}

/// What a segment's windows are filed under: its call for a card, its message
/// for a reasoning block. `None` for a segment that draws no window.
fn window_key(seg: &Segment) -> Option<Cow<'_, str>> {
    match (seg.tool_id.as_deref(), seg.msg_index) {
        (Some(tool_id), _) => Some(Cow::Borrowed(tool_id)),
        (None, Some(msg_index)) if !seg.scroll_spans.is_empty() => {
            Some(Cow::Owned(ThinkingWindow::Settled(msg_index).key()))
        }
        _ => None,
    }
}

/// Every window this segment drew, with the screen rows each took, filed under
/// `key` and a child's index after it.
fn collect_card_windows(
    seg: &Segment,
    key: &str,
    at: Placement,
    width: u16,
    viewport: Rect,
    out: &mut Vec<CardWindow>,
) {
    let chrome = seg.chrome(width);
    let inner = chrome.content_width(width);
    if inner == 0 {
        return;
    }
    for span in &seg.scroll_spans {
        // A body that fits still publishes its span, so the wheel and the bar
        // keep measuring the same rows, but it is not a window: a bar would
        // claim a remainder that is not there, and the grab region under it
        // would take presses and notches the transcript should have had.
        if span.total <= span.lines {
            continue;
        }
        let (start, rows) = seg.rows_for_lines(span.first, span.lines, width);
        let Some((y, height)) = at.clip(start, rows) else {
            continue;
        };
        out.push(CardWindow {
            key: match span.child {
                Some(index) => child_scroll_id(key, index),
                None => key.to_owned(),
            },
            body: Rect::new(viewport.x + chrome.left, y, inner, height),
            total: span.total as u32,
            position: span.offset as u32,
        });
    }
}

/// Which of the card's windows give up how many rows to hold a height of
/// `deficit` more than it just drew, in the order they were drawn.
///
/// Each window offers only what it is already holding back, so the plan runs
/// out where the buffers do and the card then shrinks — which is the honest
/// answer, and the only alternative to padding with blanks.
fn refill_plan(tl: &ToolLines, deficit: usize) -> Vec<(Option<usize>, usize)> {
    let mut owed = deficit;
    let mut plan = Vec::new();
    for span in tl
        .scroll_spans
        .iter()
        .filter(|span| span.child.is_none() && span.history_start.is_none())
    {
        let take = span.total.saturating_sub(span.lines).min(owed);
        if take == 0 {
            continue;
        }
        plan.push((span.child, take));
        owed -= take;
        if owed == 0 {
            break;
        }
    }
    plan
}

/// Where one scroll card's window sits. A card starts pinned to the tail and
/// returns to it the moment the reader scrolls back to the bottom, so the
/// default is the state most cards are in most of the time.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CardScroll {
    offset: usize,
    follow: bool,
    history_base: Option<usize>,
}

impl Default for CardScroll {
    fn default() -> Self {
        Self {
            offset: 0,
            follow: true,
            history_base: None,
        }
    }
}

impl CardScroll {
    fn at_offset(offset: usize, span: &ScrollSpan) -> Self {
        let max_offset = span.total.saturating_sub(span.extent_lines);
        let offset = offset.min(max_offset);
        let follow = offset == max_offset;
        Self {
            offset,
            follow,
            history_base: span.history_start.filter(|&base| !follow && offset >= base),
        }
    }

    fn rebase_history(&mut self, history_start: Option<usize>) -> bool {
        let Some(base) = self.history_base.filter(|_| !self.follow) else {
            return false;
        };
        let Some(history_start) = history_start.filter(|&start| start != base) else {
            return false;
        };
        self.offset = history_start.saturating_add(self.offset.saturating_sub(base));
        self.history_base = Some(history_start);
        true
    }
}

/// What the reader's first visible row is looking at, named so that
/// re-measuring the transcript cannot move it.
///
/// A raw row offset stops naming that content the moment anything above it
/// changes height, and between two frames plenty does: a tool event reshapes
/// its card as it lands, and the next frame then applies disclosure, redraws
/// every dirty card and refreshes the live rows before it resolves the
/// scroll. A segment index is no better, because a rebuild that splits an
/// instruction segment off its card inserts one and every index below shifts.
#[derive(Clone)]
struct ReadingAnchor {
    /// The message behind the row. Safe to hold across a rebuild because
    /// every path that renumbers `messages` drops the whole cache with it.
    msg_index: Option<usize>,
    /// The call the row was drawn for, which is what tells a card apart from
    /// the instruction segment that shares its message.
    tool_id: Option<String>,
    /// Rows into that segment, at the width the reader was reading at.
    rel: u16,
}

impl ReadingAnchor {
    fn names(&self, seg: &Segment) -> bool {
        seg.msg_index == self.msg_index && seg.tool_id == self.tool_id
    }
}

/// Restoring a session that was cancelled mid-tool-call replays snapshots for
/// tool ids whose messages are gone, and every snapshot row repeats the same
/// id. One line per id says everything the flood did.
#[derive(Default)]
struct DroppedSnapshots(HashSet<String>);

impl DroppedSnapshots {
    fn record(&mut self, tool_id: &str) {
        if self.0.insert(tool_id.to_owned()) {
            warn!(tool_id, "snapshot dropped: no tool message with this id");
        }
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

pub struct MessagesPanel {
    messages: Vec<DisplayMessage>,
    streaming_thinking: StreamingContent,
    streaming_text: StreamingContent,
    /// Whose words `streaming_text` currently holds. A delegated task's chat
    /// streams the instruction it is being given before any agent is attached
    /// to it, and that is the one case where the buffer is not the model's.
    streaming_role: DisplayRole,
    started_at: Instant,
    scroll_top: u32,
    auto_scroll: bool,
    scrollbar: Scrollbar,
    viewport_height: u16,
    viewport_width: u16,
    viewport_area: Rect,
    cache: SegmentCache,
    last_total_lines: u32,
    hl_worker: RenderWorker,
    theme_generation: u64,
    highlight_segment: Option<usize>,
    idle_splash: Splash,
    accent: ColorTransition,
    /// What the reader asked of a card, keyed by tool id. Absent leaves the
    /// disclosure to the view mode.
    disclosure: HashMap<String, CardState>,
    /// Which shell cards the reader put into raw view. Kept apart from
    /// `disclosure` because it is a choice about the body rather than an
    /// expansion of it, so closing the card must not forget it.
    shell_raw: HashSet<String>,
    /// Which batch children the reader opened, by parent tool id. Empty for
    /// every card nobody has clicked, which is nearly all of them.
    batch_views: BatchViewMap,
    /// What each dispatched batch child is doing, by parent tool id and child
    /// index. Live chrome rather than part of the roster: `BatchToolEntry` is
    /// persisted, and what a subagent was doing is stale the moment it stops.
    batch_child_progress: BatchProgressMap,
    task_cards: HashMap<String, TaskCard>,
    /// What each dispatched batch child has streamed so far, by parent tool id
    /// and child index. A batch keeps the live row for itself, so a child's
    /// output arrives addressed to an id no header has and is kept here until
    /// the child's own result supersedes it.
    batch_child_output: BatchLiveMap,
    /// When each running batch child started, by parent tool id and child
    /// index. Live chrome like the two above, and emptied as each child lands,
    /// so a non-empty entry is exactly a card with a clock still counting.
    batch_child_started: BatchStartedMap,
    /// One bar per window on screen, keyed exactly as `card_scroll` is, since
    /// a bar holds the anchor of a drag in progress and several windows can be
    /// visible at once. Entries for windows that are no longer drawn are swept
    /// each frame: a stale bar keeps a stale drag alive.
    card_bars: HashMap<String, Scrollbar>,
    /// Every window drawn last frame, for routing a press or a notch to the
    /// one under the pointer.
    card_windows: Vec<CardWindow>,
    /// The window the wheel reaches, set by a press inside one and released
    /// when the pointer leaves it or presses elsewhere.
    ///
    /// Without arming, a card under the pointer eats every notch aimed at the
    /// transcript behind it, which on a transcript of shell output is most of
    /// them. Hover alone is not enough of a statement of intent.
    armed_card: Option<String>,
    /// Horizontal offset per drawn diagram. Absent means unpanned, so the
    /// map stays empty for the overwhelming majority of transcripts.
    diagram_pans: HashMap<DiagramKey, u16>,
    /// Per-tool log of post-completion click rows, replayed on restore.
    lua_clicks: HashMap<String, Vec<usize>>,
    dropped_snapshots: DroppedSnapshots,
    live_bufs: HashMap<String, Arc<SharedBuf>>,
    /// Bufs of finished tools we keep polling so runtime-side warm
    /// clicks stay visible. Purely local: every finished-tool click
    /// carries a restore fallback, so we never track the runtime's
    /// warm cache.
    watched_bufs: VecDeque<(String, Arc<SharedBuf>)>,
    retained_shell_outputs: VecDeque<String>,
    tool_output_lines: ToolOutputLines,
    /// What the reader configured about tools: which never open, and how
    /// tall a scroll card's window is.
    policy: CardPolicy,
    /// Where each scroll card's window sits, by scroll id. An absent entry is
    /// pinned to the tail, which is where every card starts.
    card_scroll: HashMap<String, CardScroll>,
    lua_event_handle: EventHandle,
    restore_event_tx: Option<EventSender>,
    show_thinking: bool,
    /// Rows of a reasoning block's window. Zero draws every block whole.
    thinking_lines: u32,
    /// The live reasoning block's override, in the same tri-state as a card's.
    streaming_reasoning_open: Option<bool>,
    /// Live-turn stand-in for the persisted reasoning duration: the agent only
    /// stamps history once the turn lands, and the header wants a number while
    /// the reply is still streaming.
    thinking_started: Option<Instant>,
    view: ViewMode,
    /// Which message auto is holding open. The cache only builds segments for
    /// new messages, so the card that stops being last has to be redrawn by
    /// hand when the transcript grows past it.
    auto_open: Option<usize>,
    /// Set when a density switch drops the cache under a scrolled-up reader,
    /// and applied once the next rebuild gives the segment a height again.
    pending_scroll_segment: Option<usize>,
    clock_format: ClockFormat,
    /// One re-bake per tool per generation; `snapshot_theme_gen`
    /// only bumps when colors actually land.
    rebake_requested: HashMap<String, u64>,
    prompt_progress: Option<PromptProgress>,
    prompt_rate: PromptRate,
    hover: Option<HoverTarget>,
    /// Resolves the hashes a sent `#` names, so the transcript agrees with the
    /// composer about which of them were ever commits.
    commit_index: CommitIndex,
    /// The session's working directory, which a call's `workdir` argument is
    /// resolved against until its result says where the call ran.
    cwd: Option<Arc<Path>>,
    message_action_hits: Vec<MessageActionHit>,
    terminal_links: Vec<TerminalLink>,
    /// Cards whose message changed since the last frame and whose segment is
    /// therefore owed a rebuild. A card's lines are derived, so paying for
    /// them per event costs the card's length per event; the frame is the
    /// natural rate for something nobody can read faster than, and a chat
    /// nobody is looking at pays nothing at all.
    ///
    /// Keyed by tool id rather than segment index, so the cache moving under
    /// it cannot turn an entry into a rebuild of the wrong card.
    dirty_cards: HashSet<String>,
    /// What the reader was looking at when their position last settled, which
    /// is what the next frame puts them back on. Absent while following,
    /// which needs no anchor because the tail is one.
    held_anchor: Option<ReadingAnchor>,
    /// The rows each still-running call's card has already drawn, by tool id.
    ///
    /// A running card may grow but must not shrink, so its height is monotone
    /// and settles exactly once, at the boundary the reader already
    /// understands. What it is held against is a change of *representation* —
    /// `tool_start` swapping a streamed preview for a shorter authoritative
    /// summary, an annotation replaced by shorter text, output replaced
    /// rather than appended — and never content that has genuinely gone away.
    ///
    /// Held rows are filled from the window's own scrollback, so a card that
    /// has shown `n` rows keeps showing `n` rows of real buffered content; a
    /// card with nothing left to reveal shrinks, which is honest. Blanks are
    /// never an option: they answer clicks as a card that gave them up and
    /// leave the transcript holding space it cannot draw into.
    card_floor: HashMap<String, usize>,
}

impl MessagesPanel {
    pub fn new(ui_config: UiConfig, lua_event_handle: EventHandle) -> Self {
        let thinking = thinking_style();
        let assistant = assistant_style();
        let ms = ui_config.typewriter_ms_per_char;
        Self {
            messages: Vec::new(),
            streaming_thinking: StreamingContent::new(
                "",
                thinking.text_style,
                thinking.prefix_style,
                ms,
            )
            .wrapping(ui_config.thinking_lines > 0),
            streaming_text: StreamingContent::new(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
                ms,
            ),
            streaming_role: DisplayRole::Assistant,
            started_at: Instant::now(),
            scroll_top: u32::MAX,
            auto_scroll: true,
            scrollbar: Scrollbar::default(),
            viewport_height: 24,
            viewport_width: crossterm::terminal::size().map_or(80, |(w, _)| w.saturating_sub(1)),
            viewport_area: Rect::default(),
            cache: SegmentCache::new(),
            last_total_lines: 0,
            hl_worker: RenderWorker::new(),
            theme_generation: theme::generation(),
            highlight_segment: None,
            idle_splash: Splash::new(ui_config.splash_animation),
            accent: ColorTransition::new(theme::current().mode_build),
            disclosure: HashMap::new(),
            shell_raw: HashSet::new(),
            batch_views: BatchViewMap::new(),
            batch_child_progress: BatchProgressMap::new(),
            task_cards: HashMap::new(),
            batch_child_output: BatchLiveMap::new(),
            batch_child_started: BatchStartedMap::new(),
            card_bars: HashMap::new(),
            card_windows: Vec::new(),
            armed_card: None,
            diagram_pans: HashMap::new(),
            lua_clicks: HashMap::new(),
            dropped_snapshots: DroppedSnapshots::default(),
            live_bufs: HashMap::new(),
            watched_bufs: VecDeque::new(),
            retained_shell_outputs: VecDeque::new(),
            tool_output_lines: ui_config.tool_output_lines,
            policy: CardPolicy {
                always_collapsed: ui_config.always_collapsed.into(),
                scroll_card_lines: ui_config.scroll_card_lines,
                compact: ViewMode::default() == ViewMode::Compact,
                expanded: ViewMode::default() == ViewMode::Expanded,
            },
            card_scroll: HashMap::new(),
            lua_event_handle,
            restore_event_tx: None,
            show_thinking: ui_config.show_thinking,
            thinking_lines: ui_config.thinking_lines,
            streaming_reasoning_open: None,
            thinking_started: None,
            view: ViewMode::default(),
            auto_open: None,
            pending_scroll_segment: None,
            clock_format: ui_config.clock_format,
            rebake_requested: HashMap::new(),
            prompt_progress: None,
            prompt_rate: PromptRate::default(),
            hover: None,
            commit_index: CommitIndex::default(),
            cwd: None,
            message_action_hits: Vec::new(),
            terminal_links: Vec::new(),
            dirty_cards: HashSet::new(),
            held_anchor: None,
            card_floor: HashMap::new(),
        }
    }

    pub fn set_restore_channel(&mut self, event_tx: Option<EventSender>) {
        self.restore_event_tx = event_tx;
    }

    /// Switching mode drops every per-card choice, so the shortcut that
    /// changes the mode really does move the whole transcript.
    pub fn set_view(&mut self, view: ViewMode) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.policy.compact = view == ViewMode::Compact;
        self.policy.expanded = view == ViewMode::Expanded;
        // The modes draw wildly different cards, so a height held against one
        // of them says nothing about the next.
        self.card_floor.clear();
        self.clear_hover();
        self.disclosure.clear();
        self.streaming_reasoning_open = None;
        self.auto_open = None;
        for msg in &mut self.messages {
            msg.body_open = None;
        }
        // Anchor before the cache goes: the modes have wildly different
        // heights, so a raw line offset would land anywhere.
        let anchor = self.cache.anchor_at(self.scroll_top, self.viewport_width);
        self.cache.clear();
        // The next frame builds every segment from scratch, so an owed redraw
        // is already paid for and would only shift the anchor index below.
        self.dirty_cards.clear();
        if let Some((seg_idx, _)) = anchor.filter(|_| !self.auto_scroll) {
            self.pending_scroll_segment = Some(seg_idx);
        }
    }

    /// Compact and auto both draw a call as one row of a list. Expanded gives
    /// every call a card of its own.
    fn compact(&self) -> bool {
        self.view != ViewMode::Expanded
    }

    /// Whether this tool's card never opens on its own. Configured by bare
    /// name, so a call the same tool makes through an MCP server matches too.
    fn stays_collapsed(&self, tool: &str) -> bool {
        self.policy.stays_collapsed(tool)
    }

    /// Whether this call is drawn as a one-line row rather than a card. The
    /// mode answers for most tools; an always-collapsed one answers for
    /// itself, so expanded draws it as the row it would be anywhere else.
    fn draws_compact(&self, tool: &str) -> bool {
        self.compact() || self.stays_collapsed(tool)
    }

    fn card_draws_compact(&self, tool_id: &str) -> bool {
        self.tool_card(tool_id).map_or_else(
            || self.compact(),
            |(_, role)| self.draws_compact(&role.name),
        )
    }

    /// The card at the end of the transcript is the one being written. Live
    /// reasoning and text draw after every settled card, so while either is
    /// running nothing settled is last.
    fn is_latest(&self, msg_index: usize) -> bool {
        self.streaming_thinking.is_empty()
            && self.streaming_text.is_empty()
            && msg_index + 1 == self.messages.len()
    }

    /// Whether the mode alone draws this call's body.
    ///
    /// Compact answers for every tool and is asked first. The reader asked for
    /// a list of what ran, and a mode that kept drawing the bodies of writes,
    /// isolated calls, orchestrators and every unclassified MCP tool was a
    /// list in name only. A body that is the only record of a change is a
    /// click away rather than hidden, and it opens whole.
    ///
    /// A batch is the one exception, because its body *is* the list of the
    /// calls it made. Folded it says nothing at all, so the mode would be
    /// hiding the very thing it is for. Its children fold instead.
    ///
    /// Elsewhere the older rule holds: a call whose body is the only record of
    /// what it did stays open, and an always-collapsed tool is the other
    /// extreme that no mode opens.
    fn opens_by_default(&self, role: &ToolRole, msg_index: usize) -> bool {
        if self.view == ViewMode::Compact {
            return &*role.name == BATCH_TOOL_NAME;
        }
        if !role.is_collapsible() {
            return true;
        }
        if self.stays_collapsed(&role.name) {
            return false;
        }
        self.view == ViewMode::Expanded || self.is_latest(msg_index)
    }

    /// Resolved from the message rather than tracked alongside it, so a
    /// restored session cannot disagree with a live one.
    fn tool_card(&self, tool_id: &str) -> Option<(usize, &ToolRole)> {
        self.messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, msg)| match &msg.role {
                DisplayRole::Tool(t) if t.id == tool_id => Some((i, t.as_ref())),
                _ => None,
            })
    }

    fn card_opens_by_default(&self, tool_id: &str) -> bool {
        self.tool_card(tool_id)
            .is_some_and(|(idx, role)| self.opens_by_default(role, idx))
    }

    /// Whether a click can take this card back to its header. The only bar is
    /// that there is a row to fall back to, which an expanded transcript does
    /// not give a card of its own.
    ///
    /// A write clears that bar. It cannot be folded *by mode* — hiding a diff
    /// nobody asked to hide loses the change — but the reader asking for it is
    /// a different thing, and the row still names the file and carries the
    /// stat the diff would have shown.
    fn card_can_close(&self, tool_id: &str) -> bool {
        self.card_draws_compact(tool_id) && self.tool_card(tool_id).is_some()
    }

    /// Whether this card draws its body in a fixed window rather than
    /// abridging it to a budget.
    fn card_scrolls(&self, tool_id: &str) -> bool {
        self.tool_card(tool_id)
            .is_some_and(|(_, role)| self.rctx(&role.name, tool_id).card_scroll.is_some())
    }

    /// `None` means header-only. A card the reader has not spoken about rests
    /// within the tool's row budget; the budget is never what a click opens to,
    /// so an asked-for card is always whole.
    ///
    /// A scroll card is the exception: its window is the whole of what it
    /// shows, so it is never `full` and a click that would have opened one
    /// closes it instead.
    fn tool_expansion(&self, tool_id: &str, open_by_default: bool) -> Option<Disclosure> {
        let full = match self.disclosure.get(tool_id) {
            Some(CardState::Closed) => return None,
            Some(CardState::Full) => !self.card_scrolls(tool_id),
            None if open_by_default => false,
            None => return None,
        };
        Some(Disclosure {
            full,
            shell_raw: self.shell_raw.contains(tool_id),
        })
    }

    /// Re-pins a card's window to the tail, which is also what the reader
    /// gets by scrolling back down to it.
    fn follow_card(&mut self, tool_id: &str) {
        self.card_scroll
            .insert(tool_id.to_owned(), CardScroll::default());
        self.rebuild_expanded_tool(tool_id);
    }

    /// Moves one scroll card's window and reports the notches it could not
    /// use, which the transcript then scrolls by. A card that swallowed a
    /// whole burst at its own edge would trap the reader inside it.
    ///
    /// `delta` is positive upwards, as everywhere else in the app.
    fn scroll_window(&mut self, key: &str, delta: i32) -> i32 {
        if let Some(block) = ThinkingWindow::parse(key) {
            let Some(span) = self.thinking_span(block) else {
                return delta;
            };
            let Some(left) = self.step_window(block.key(), &span, delta) else {
                return delta;
            };
            self.redraw_thinking(block);
            return left;
        }
        let Some((_, rebuild)) = self.window_body(key) else {
            return delta;
        };
        self.move_window(key.to_owned(), &rebuild, delta)
    }

    /// The body a scroll key names and the card to redraw for it: a card's own
    /// body, or one child of it. The child suffix is only taken when it
    /// resolves, so a tool whose id happens to carry one still names itself.
    ///
    /// The two counts answer different questions. Whether there is a window at
    /// all is a question about the tool, which only the message can answer;
    /// how far that window can travel is a question about rows, which only the
    /// build that drew them can answer. Sizing the travel from the source
    /// instead puts the wheel and the bar in different units the moment a body
    /// is wrapped or rendered as anything but plain text.
    ///
    /// So a body the last build recorded no extent for is one there is
    /// nothing to scroll *yet*, and it says so. Standing the source count in
    /// for the missing one is the unit mix this split exists to prevent: the
    /// travel it sizes runs out part way up a wrapped body, stranding rows the
    /// bar says are there and spilling the rest of the burst to the transcript.
    fn window_body(&self, key: &str) -> Option<(usize, String)> {
        if let Some((parent, index)) = split_child_scroll_id(key)
            && self.child_body_lines(parent, index).is_some()
        {
            let rows = self.window_rows(parent, Some(index))?;
            return Some((rows, parent.to_owned()));
        }
        self.card_body_lines(key)?;
        let rows = self.window_rows(key, None)?;
        Some((rows, key.to_owned()))
    }

    /// The extent the last build recorded beside the window it drew, which is
    /// the count the bar's track already spans. Absent until the card has been
    /// built once, and for a card drawn without a window at all.
    fn window_rows(&self, tool_id: &str, child: Option<usize>) -> Option<usize> {
        self.window_span(tool_id, child).map(|span| span.total)
    }

    fn window_span(&self, tool_id: &str, child: Option<usize>) -> Option<&ScrollSpan> {
        let seg = self.cache.get(self.cache.find_by_tool_id(tool_id)?)?;
        seg.scroll_spans.iter().find(|span| span.child == child)
    }

    fn keyed_window_span(&self, key: &str, tool_id: &str) -> Option<&ScrollSpan> {
        let child = split_child_scroll_id(key)
            .filter(|(parent, _)| *parent == tool_id)
            .map(|(_, index)| index);
        self.window_span(tool_id, child)
    }

    /// Puts a window at an absolute offset, which is what dragging a bar asks
    /// for. Landing on the last row re-arms following, exactly as scrolling
    /// back to the bottom does.
    fn jump_window(&mut self, key: &str, offset: usize) {
        if let Some(block) = ThinkingWindow::parse(key) {
            if let Some(span) = self.thinking_span(block) {
                let scroll = CardScroll::at_offset(offset, &span);
                self.card_scroll.insert(block.key(), scroll);
                self.redraw_thinking(block);
            }
            return;
        }
        let Some((_, rebuild)) = self.window_body(key) else {
            return;
        };
        let Some(span) = self.keyed_window_span(key, &rebuild) else {
            return;
        };
        let scroll = CardScroll::at_offset(offset, span);
        self.card_scroll.insert(key.to_owned(), scroll);
        self.rebuild_expanded_tool(&rebuild);
    }

    /// `rebuild` is the card to redraw, which for a child is the parent whose
    /// body it is drawn inside.
    fn move_window(&mut self, key: String, rebuild: &str, delta: i32) -> i32 {
        let Some(span) = self.keyed_window_span(&key, rebuild).copied() else {
            return delta;
        };
        let Some(left) = self.step_window(key, &span, delta) else {
            return delta;
        };
        self.rebuild_expanded_tool(rebuild);
        left
    }

    /// Moves the window filed under `key` by `delta` notches over `span`, and
    /// reports the notches it could not use. `None` when it did not move, so
    /// the owner has nothing to redraw and the burst is the transcript's.
    fn step_window(&mut self, key: String, span: &ScrollSpan, delta: i32) -> Option<i32> {
        let max_offset = span.total.saturating_sub(span.extent_lines);
        if max_offset == 0 || delta == 0 {
            return None;
        }
        let offset = span.offset;
        // Upwards is towards the start of the body, which is a lower offset.
        let wanted = offset as i64 - i64::from(delta);
        let landed = wanted.clamp(0, max_offset as i64) as usize;
        let used = offset.abs_diff(landed) as i32;
        let scroll = CardScroll::at_offset(landed, span);
        if used == 0 && self.card_scroll.get(&key).copied().unwrap_or_default() == scroll {
            return None;
        }
        self.card_scroll.insert(key, scroll);
        Some((delta.abs() - used) * delta.signum())
    }

    /// The window a reasoning block's body is drawn in, or `None` when the
    /// reader turned windows off and every block draws whole.
    fn thinking_window(&self, block: ThinkingWindow) -> Option<ScrollWindow> {
        (self.thinking_lines > 0).then(|| {
            let at = self
                .card_scroll
                .get(&block.key())
                .copied()
                .unwrap_or_default();
            ScrollWindow {
                height: self.thinking_lines as usize,
                offset: at.offset,
                follow: at.follow,
            }
        })
    }

    /// Where a reasoning block's window was last drawn. A settled block keeps
    /// it on its segment; the live one has no segment, so it is rebuilt from
    /// the same rows the frame paints.
    fn thinking_span(&self, block: ThinkingWindow) -> Option<ScrollSpan> {
        match block {
            ThinkingWindow::Live if self.streaming_thinking_collapsed() => None,
            ThinkingWindow::Live => self
                .streaming_thinking_segment()
                .scroll_spans
                .first()
                .copied(),
            ThinkingWindow::Settled(msg_index) => self
                .cache
                .segments()
                .iter()
                .find(|seg| seg.msg_index == Some(msg_index) && seg.tool_id.is_none())?
                .scroll_spans
                .first()
                .copied(),
        }
    }

    /// Only a settled block has lines to redraw: the live one is rebuilt by
    /// every frame anyway.
    fn redraw_thinking(&mut self, block: ThinkingWindow) {
        if let ThinkingWindow::Settled(msg_index) = block {
            self.rebuild_thinking_segment(msg_index, self.viewport_width);
        }
    }

    /// Re-pins a reasoning block's window to its newest rows, which is where an
    /// absent entry rests.
    fn follow_thinking(&mut self, block: ThinkingWindow) {
        self.card_scroll.remove(&block.key());
        self.redraw_thinking(block);
    }

    /// Files one window's place, bar and arming under a new key, so a block
    /// that changes identity keeps the reader where they were.
    fn rekey_window(&mut self, from: &str, to: String) {
        if let Some(scroll) = self.card_scroll.remove(from) {
            self.card_scroll.insert(to.clone(), scroll);
        }
        if let Some(bar) = self.card_bars.remove(from) {
            self.card_bars.insert(to.clone(), bar);
        }
        for window in self
            .card_windows
            .iter_mut()
            .filter(|window| window.key == from)
        {
            window.key.clone_from(&to);
        }
        if self.armed_card.as_deref() == Some(from) {
            self.armed_card = Some(to);
        }
    }

    /// Drops every reasoning window `stale` names, wherever a window is kept.
    fn forget_thinking_windows(&mut self, stale: impl Fn(ThinkingWindow) -> bool) {
        let keep = |key: &str| !ThinkingWindow::parse(key).is_some_and(&stale);
        self.card_scroll.retain(|key, _| keep(key));
        self.card_bars.retain(|key, _| keep(key));
        self.card_windows.retain(|window| keep(&window.key));
        if self.armed_card.as_deref().is_some_and(|key| !keep(key)) {
            self.armed_card = None;
        }
    }

    fn clear_history_anchors(&mut self, tool_id: &str) {
        for (key, scroll) in &mut self.card_scroll {
            if key == tool_id
                || split_child_scroll_id(key).is_some_and(|(parent, _)| parent == tool_id)
            {
                scroll.history_base = None;
            }
        }
    }

    /// How many lines this card's body holds, or `None` when it is not a
    /// scroll card or has nothing to scroll.
    fn card_body_lines(&self, tool_id: &str) -> Option<usize> {
        if !self.card_scrolls(tool_id) {
            return None;
        }
        let msg = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))?;
        if let Some(snapshot) = msg.render_snapshot.as_ref() {
            return Some(snapshot.lines.len());
        }
        if let Some(live) = msg.live_body.as_ref() {
            // A streaming script is drawn whole, so there is no window over it
            // to move. Sizing one from it would leave the reader's offset
            // waiting for the output, which then opens part-scrolled.
            let script = msg.role.tool_name().is_some_and(draws_live_script);
            return (!script).then(|| live.lines().count());
        }
        let text = msg
            .tool_output
            .as_deref()
            .map(ToolOutput::as_text)
            .or_else(|| msg.text.split_once('\n').map(|(_, body)| body.to_owned()));
        text.map(|text| text.lines().count())
            .or_else(|| self.window_rows(tool_id, None))
    }

    fn child_body_lines(&self, parent_id: &str, index: usize) -> Option<usize> {
        let (msg_idx, _) = self.tool_card(parent_id)?;
        let ToolOutput::Batch { entries, .. } = self.messages[msg_idx].tool_output.as_deref()?
        else {
            return None;
        };
        let entry = entries.get(index)?;
        self.policy
            .scrolls(&entry.tool)
            .then(|| match entry.output.as_ref() {
                Some(output) => output.as_text().lines().count(),
                // A child that has not answered is drawn from what it has
                // streamed, so that is what its window moves over. Counting
                // only settled output leaves a running command unscrollable
                // for exactly as long as there is a reason to scroll it.
                None => self
                    .batch_child_stream(parent_id, index)
                    .map_or(0, |tail| tail.lines().count()),
            })
    }

    /// Shows the whole body, since that is the only thing a click opens to.
    fn open_card(&mut self, tool_id: &str) {
        self.disclosure.insert(tool_id.to_owned(), CardState::Full);
        self.rebuild_expanded_tool(tool_id);
    }

    /// Auto keeps the card being written open and closes the one it replaced.
    /// The cache only builds segments for messages it has not seen, so the
    /// card that stops being last has to be redrawn by hand.
    fn follow_latest(&mut self) {
        let latest = (self.view == ViewMode::Auto)
            .then(|| self.auto_governed_tail())
            .flatten();
        if latest == self.auto_open {
            return;
        }
        let previous = mem::replace(&mut self.auto_open, latest);
        for idx in previous.into_iter().chain(latest) {
            self.rebuild_card(idx);
        }
    }

    /// The last card, when the mode is the only thing deciding whether it is
    /// open. A card the reader chose for, or one that cannot close at all,
    /// does not move when the transcript grows past it. Reasoning is absent
    /// because no mode closes it.
    fn auto_governed_tail(&self) -> Option<usize> {
        let idx = self.messages.len().checked_sub(1)?;
        if !self.is_latest(idx) {
            return None;
        }
        let governed = match &self.messages[idx].role {
            DisplayRole::Tool(t) => {
                t.is_collapsible()
                    && !self.stays_collapsed(&t.name)
                    && !self.disclosure.contains_key(&t.id)
            }
            _ => false,
        };
        governed.then_some(idx)
    }

    fn rebuild_card(&mut self, msg_index: usize) {
        let Some(msg) = self.messages.get(msg_index) else {
            return;
        };
        match &msg.role {
            DisplayRole::Tool(t) => {
                let id = t.id.clone();
                self.rebuild_tool_lines(&id);
            }
            DisplayRole::Thinking => {
                self.rebuild_thinking_segment(msg_index, self.viewport_width);
            }
            _ => {}
        }
    }

    pub(crate) fn card_closed(&self, tool_id: &str) -> bool {
        self.tool_expansion(tool_id, self.card_opens_by_default(tool_id))
            .is_none()
    }

    /// Whether a click on the row would change anything. Hover feedback and
    /// the click share this, or a row highlights and then ignores the press.
    fn tool_click_acts(&self, tool_id: &str, truncation: bool) -> bool {
        let exp = self.tool_expansion(tool_id, self.card_opens_by_default(tool_id));
        (exp.is_some() && self.card_can_close(tool_id)) || truncation || exp.is_some_and(|d| d.full)
    }

    /// Hands back the index of the message, which [`Self::replace`] needs to
    /// correct it later.
    pub fn push(&mut self, msg: DisplayMessage) -> usize {
        self.messages.push(msg);
        self.messages.len() - 1
    }

    /// Drops the whole segment cache, so keep it for one-off corrections and
    /// never for streaming. Marking the message stale is not enough: only the
    /// segments the viewport reaches get reflowed, so a fix above it would
    /// keep painting the old bubble.
    pub fn replace(&mut self, index: usize, msg: DisplayMessage) {
        let Some(slot) = self.messages.get_mut(index) else {
            return;
        };
        *slot = msg;
        self.cache.clear();
    }

    pub fn remove(&mut self, index: usize) {
        if index >= self.messages.len() {
            return;
        }
        self.messages.remove(index);
        self.cache.clear();
        self.auto_open = None;
        // Every index above `index` now names a different message, which is
        // exactly what the anchor and a reasoning window's key are holding.
        self.held_anchor = None;
        self.forget_thinking_windows(
            |block| matches!(block, ThinkingWindow::Settled(msg_index) if msg_index >= index),
        );
    }

    /// Drops the newest harness notice reading `text`. Found by content rather
    /// than by a remembered index, so a transcript that was reloaded or grew
    /// in between still loses the right row, or none at all.
    pub fn remove_notice(&mut self, text: &str) {
        if let Some(index) = self
            .messages
            .iter()
            .rposition(|msg| msg.role == DisplayRole::Notice && msg.text == text)
        {
            self.remove(index);
        }
    }

    pub fn load_messages(&mut self, mut msgs: Vec<DisplayMessage>) {
        for msg in &mut msgs {
            msg.body_open = None;
        }
        self.dropped_snapshots.clear();
        self.messages = msgs;
        self.cache.clear();
        self.auto_open = None;
        self.held_anchor = None;
        self.card_floor.clear();
        self.disclosure.clear();
        self.shell_raw.clear();
        self.batch_views.clear();
        self.batch_child_progress.clear();
        self.task_cards.clear();
        self.batch_child_output.clear();
        self.batch_child_started.clear();
        for scroll in self.card_scroll.values_mut() {
            scroll.history_base = None;
        }
        self.forget_thinking_windows(|block| matches!(block, ThinkingWindow::Settled(_)));
        self.card_bars.clear();
        self.card_windows.clear();
        self.armed_card = None;
        self.lua_clicks.clear();
        self.live_bufs.clear();
        self.dirty_cards.clear();
        self.watched_bufs.clear();
        self.retained_shell_outputs.clear();
        self.rebake_requested.clear();
        self.highlight_segment = None;
        self.streaming_reasoning_open = None;
    }

    pub fn bind_sources(&mut self, source_messages: &[DisplayMessage]) {
        for (row, source) in matched_sources(&self.messages, source_messages) {
            self.messages[row].source = source;
        }
    }

    pub fn thinking_delta(&mut self, text: &str) {
        self.clear_hover();
        self.thinking_started.get_or_insert_with(Instant::now);
        self.streaming_thinking.push(text);
    }

    pub fn thinking_boundary(&mut self) {
        self.clear_hover();
        self.flush_thinking();
    }

    pub fn text_delta(&mut self, text: &str) {
        self.clear_hover();
        self.flush_thinking();
        self.stream_as(DisplayRole::Assistant);
        self.streaming_text.push(text);
    }

    /// Reconciles the open buffer with the text the agent actually kept, so a
    /// card can never show less than the session file holds. A summary gains
    /// its requirements section once extraction finishes, which is after its
    /// own stream closed.
    ///
    /// The common case is an extension, which only has to be pushed; a buffer
    /// that is not a prefix of the final text is replaced outright, since a
    /// delta cannot take characters back.
    pub fn adopt_final_text(&mut self, text: &str) {
        if self.streaming_role != DisplayRole::Assistant || self.streaming_text.is_empty() {
            return;
        }
        let Some(tail) = text.strip_prefix(self.streaming_text.buffer()) else {
            self.streaming_text.clear();
            self.streaming_text.push(text);
            return;
        };
        self.streaming_text.push(tail);
    }

    /// Grows the instruction a delegated task is being given while the call
    /// that will carry it is still being written. The chat it lands in has no
    /// agent behind it yet, so nothing else can be streaming into the same
    /// buffer.
    pub fn prompt_delta(&mut self, text: &str) {
        self.clear_hover();
        self.stream_as(DisplayRole::User);
        self.streaming_text.push(text);
        self.enable_auto_scroll();
    }

    /// Closes the buffer before it changes hands, so one bubble can never hold
    /// two speakers.
    fn stream_as(&mut self, role: DisplayRole) {
        if self.streaming_role == role {
            return;
        }
        self.flush();
        self.streaming_role = role;
    }

    /// The chrome the streaming buffer draws under, which follows whose words
    /// it holds.
    fn streaming_kind(&self) -> SegmentKind {
        segment_kind(&self.streaming_role)
    }

    pub fn tool_pending(&mut self, id: String, name: &str) {
        if self.tool_card(&id).is_some() {
            return;
        }
        self.flush();
        let role = DisplayRole::Tool(Box::new(ToolRole {
            id,
            status: ToolStatus::InProgress,
            name: Arc::from(name),
            effect: ToolEffect::default(),
        }));
        let mut msg = DisplayMessage::new(role, String::new());
        msg.tool_preview_pending = true;
        msg.tool_stage = Some(CallStage::Drafting);
        msg.timestamp = Some(format_timestamp_now(self.clock_format));
        self.messages.push(msg);
    }

    /// The call has moved past `stage`: its arguments closed, or its prompt
    /// was answered. A card already somewhere else keeps where it is. A batch
    /// child's row is left to the progress the move produces, which is never
    /// far behind and can only move it forwards.
    pub fn leave_stage(&mut self, tool_id: &str, stage: CallStage) {
        let Some(msg) = self.find_tool_msg_mut(tool_id) else {
            return;
        };
        if msg.tool_stage != Some(stage) {
            return;
        }
        msg.tool_stage = None;
        self.mark_card_dirty(tool_id);
    }

    /// The call waits on the reader's answer to a permission prompt, which is
    /// asked under the call's own id: a card's, or a batch child's. A child
    /// already running asked from inside its run, the way an MCP call does,
    /// and its row says more than the wait would.
    pub fn await_approval(&mut self, tool_id: &str) {
        if self.tool_in_progress(tool_id) {
            self.update_tool(tool_id, |msg| {
                msg.tool_stage = Some(CallStage::AwaitingApproval);
            });
        } else if let Some((parent, index)) = batch_child_id(tool_id)
            && self.batch_child_running(parent, index)
            && let Some(msg) = self.find_tool_msg_mut(parent)
            && let Some(output) = &mut msg.tool_output
            && let ToolOutput::Batch { entries, .. } = Arc::make_mut(output)
            && entries[index].status < BatchToolStatus::AwaitingApproval
        {
            entries[index].status = BatchToolStatus::AwaitingApproval;
            self.mark_card_dirty(parent);
        }
    }

    /// What a still-streaming call has revealed so far: the header it has
    /// earned, and how much of a file body has arrived. `ToolStart` replaces
    /// both with the real summary, so this only ever fills the gap between the
    /// call being announced and its arguments being complete.
    pub fn tool_input_preview(
        &mut self,
        tool_id: &str,
        header: Option<String>,
        size: Option<String>,
    ) {
        if header.is_none() && size.is_none() {
            return;
        }
        let Some(msg) = self.find_tool_msg_mut(tool_id) else {
            return;
        };
        if !msg.tool_preview_pending || !Self::is_running(msg) {
            return;
        }
        if let Some(header) = header {
            msg.text = header;
        }
        if let Some(size) = size {
            msg.annotation = Some(size);
        }
        self.mark_card_dirty(tool_id);
    }

    pub fn tool_input_roster(&mut self, tool_id: &str, entries: Option<Vec<BatchToolEntry>>) {
        let Some(entries) = entries else {
            return;
        };
        let Some(msg) = self.find_tool_msg_mut(tool_id) else {
            return;
        };
        if !msg.tool_preview_pending || !Self::is_running(msg) {
            return;
        }
        merge_batch_snapshot(msg, entries, String::new());
        self.mark_card_dirty(tool_id);
    }

    /// The file a still-streaming write is spelling out. `ToolStart` drops it
    /// for the call's real output, so it only ever fills the wait.
    ///
    /// Only the text is kept here. The card is drawn from it once per frame
    /// by [`Self::flush_dirty_cards`], because the body is drawn whole and a
    /// fragment arrives per token.
    pub fn tool_input_body(&mut self, tool_id: &str, body: Option<String>) {
        let Some(body) = body else {
            return;
        };
        let Some(msg) = self.find_tool_msg_mut(tool_id) else {
            return;
        };
        if !msg.tool_preview_pending || !Self::is_running(msg) {
            return;
        }
        msg.live_body.get_or_insert_default().push_str(&body);
        self.mark_card_dirty(tool_id);
    }

    /// Owes `tool_id` a redraw at the next frame. The message itself is always
    /// written as the event lands; only the lines derived from it wait, so
    /// everything that reads the transcript still sees the latest state.
    fn mark_card_dirty(&mut self, tool_id: &str) {
        self.dirty_cards.insert(tool_id.to_owned());
    }

    /// Redraws every card that changed since the last frame. Called from
    /// `view`, so a burst of live events costs each card its length once for
    /// the frame that shows it and nothing at all while nobody is looking.
    ///
    /// Deliberately unconditional: the reflow pass only reaches the segments
    /// around the viewport, so anything cleverer would leave an off-screen
    /// card holding lines that no later frame goes back for.
    ///
    /// Hover-preserving, like the other per-frame refreshes: this runs before
    /// the frame reads hover, so clearing it here would cancel the reader's
    /// pointer for as long as anything in the chat is still streaming.
    fn flush_dirty_cards(&mut self) {
        for tool_id in mem::take(&mut self.dirty_cards) {
            self.rebuild_tool_lines(&tool_id);
        }
    }

    pub fn tool_start(&mut self, event: ToolStartEvent) {
        self.dirty_cards.remove(&event.id);
        if let Some(msg) = self.find_tool_msg_mut(&event.id) {
            if !Self::is_running(msg) {
                return;
            }
            msg.tool_preview_pending = false;
            msg.tool_stage = None;
            // Not reset: a speculative call resends its start on adoption, and
            // the clock belongs to the run rather than to the announcement.
            msg.tool_started.get_or_insert_with(Instant::now);
            if let DisplayRole::Tool(t) = &mut msg.role {
                t.name = Arc::clone(&event.tool);
                t.effect = event.effect;
            }
            msg.text = event.summary;
            msg.tool_input = event.input.map(Arc::new);
            msg.tool_raw_input = event.raw_input.map(Arc::new);
            match event.output {
                Some(ToolOutput::Batch { entries, text }) => {
                    merge_batch_snapshot(msg, entries, text)
                }
                Some(output) => msg.tool_output = Some(Arc::new(output)),
                None => {}
            }
            msg.live_body = None;
            msg.annotation = event.annotation;
            msg.render_header = event.render_header;
            self.settle_batch_snapshot(&event.id);
            self.sync_child_clocks(&event.id);
            self.rebuild_tool_segment(&event.id);
            return;
        }
        self.flush();
        let id = event.id.clone();
        let mut msg = DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: event.id,
                status: ToolStatus::InProgress,
                name: Arc::clone(&event.tool),
                effect: event.effect,
            })),
            event.summary,
        );
        msg.tool_input = event.input.map(Arc::new);
        msg.tool_raw_input = event.raw_input.map(Arc::new);
        msg.tool_output = event.output.map(Arc::new);
        msg.annotation = event.annotation;
        msg.render_header = event.render_header;
        msg.timestamp = Some(format_timestamp_now(self.clock_format));
        msg.tool_started = Some(Instant::now());
        self.messages.push(msg);
        self.sync_child_clocks(&id);
    }

    pub fn batch_progress(&mut self, tool_id: &str, index: usize, entry: BatchToolEntry) -> bool {
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return false;
        };
        if !Self::is_running(msg) {
            return false;
        }
        let Some(ToolOutput::Batch { entries, .. }) = msg.tool_output.as_deref() else {
            return false;
        };
        if index >= entries.len()
            || entries[index].status.is_terminal()
            || entry.status < entries[index].status
        {
            return false;
        }
        let terminal = entry.status.is_terminal();
        let mut entries = entries.clone();
        entries[index] = entry;
        msg.tool_output = Some(Arc::new(ToolOutput::Batch {
            entries,
            text: String::new(),
        }));
        self.sync_child_clocks(tool_id);
        // What it was doing is stale the moment it stops, but what it did is
        // the only record of work its output does not show.
        if terminal {
            self.settle_child_progress(tool_id, index);
            self.forget_child_output(tool_id, index);
        }
        self.mark_card_dirty(tool_id);
        true
    }

    pub(crate) fn task_card_update(&mut self, mut card: TaskCard) -> bool {
        card.result = None;
        card.result_preview = None;
        card.result_truncated = false;
        card.reports.clear();
        card.reports_truncated = false;
        if self.task_cards.get(&card.call_id) == Some(&card) {
            return false;
        }
        let call_id = card.call_id.clone();
        let active = card.active();
        let shell = card.kind == JobKind::Shell;
        let dependents: Vec<_> = self
            .messages
            .iter()
            .filter_map(|message| {
                message
                    .tool_output
                    .as_deref()
                    .filter(|output| task_card::contains_invocation(output, &card.invocation_id))
                    .and(message.role.tool_id())
                    .map(str::to_owned)
            })
            .collect();
        self.task_cards.insert(call_id.clone(), card);
        let parent = batch_child_id(&call_id);
        if !active {
            if shell {
                self.live_bufs.remove(&call_id);
                self.stop_watching(&call_id);
                if let Some((parent, index)) = parent {
                    self.forget_child_output(parent, index);
                } else if let Some(msg) = self.find_tool_msg_mut(&call_id) {
                    msg.live_output = None;
                    msg.render_snapshot = None;
                }
            }
            if let Some((parent, index)) = parent {
                self.settle_child_progress(parent, index);
            } else if let Some(msg) = self.find_tool_msg_mut(&call_id)
                && let Some(progress) = &mut msg.progress
            {
                progress.settle();
            }
        }
        if let Some(root) = self.root_tool_id(&call_id) {
            self.mark_card_dirty(root);
        }
        for dependent in dependents {
            self.mark_card_dirty(&dependent);
        }
        true
    }

    pub(crate) fn reconcile_task_cards(&mut self, runtime: &BackgroundTasks) -> bool {
        let mut identities: HashMap<String, String> = self
            .task_cards
            .values()
            .map(|task| (task.invocation_id.clone(), task.task_id.clone()))
            .collect();
        for message in &self.messages {
            if let Some(output) = message.tool_output.as_deref() {
                collect_task_identities(output, &mut identities);
            }
        }
        let mut changed = false;
        for (invocation, task_id) in identities {
            if let Some(card) = runtime.resident_invocation_status(&task_id, &invocation) {
                changed |= self.task_card_update(card);
            }
        }
        changed
    }

    pub(crate) fn task_hit_at(&self, row: u16, area: Rect) -> Option<String> {
        if area.height == 0 {
            return None;
        }
        let width = self.viewport_width;
        let doc_row = self.doc_row(row, area);
        let (_, segment, start) = self.cache.segment_at_row(doc_row, width)?;
        let rel = u16::try_from(doc_row - start).ok()?;
        if rel < segment.chrome(width).margin_top {
            return None;
        }
        if let Some(message) = segment.msg_index.and_then(|index| self.messages.get(index))
            && let DisplayRole::TaskDelivery(origin) = &message.role
        {
            return (!task_card::is_shell_delivery(origin, &message.text))
                .then(|| origin.task_id.clone());
        }
        let agent_target =
            |task: &TaskCard| (task.kind == JobKind::Agent).then(|| task.task_id.clone());
        let call_id = segment.tool_id.as_deref()?;
        let target = segment.row_target_at(rel, width);
        let output = self
            .messages
            .iter()
            .rfind(|msg| msg.role.tool_id() == Some(call_id))?
            .tool_output
            .as_deref();
        if let Some(output) = output
            && let Some(target) = target
            && let Some(index) = task_card::target_index(output, target)
        {
            return task_card::task_at(output, index).and_then(agent_target);
        }
        match output {
            Some(ToolOutput::Tasks(tasks)) => tasks
                .first()
                .filter(|_| tasks.len() == 1)
                .and_then(agent_target),
            Some(ToolOutput::Batch { entries, .. }) => {
                let target = target?;
                let index = target.index();
                match entries.get(index)?.output.as_ref() {
                    Some(ToolOutput::Tasks(tasks)) => {
                        let task = match target {
                            RowTarget::Task { task, .. } => task,
                            RowTarget::Item(_) if tasks.len() == 1 => 0,
                            _ => return None,
                        };
                        tasks.get(task).and_then(agent_target)
                    }
                    _ => self
                        .task_cards
                        .get(&format!("{call_id}:{index}"))
                        .and_then(agent_target),
                }
            }
            _ => self.task_cards.get(call_id).and_then(agent_target),
        }
    }

    /// The firing whose delivery a click at `row` landed on. The whole row
    /// answers, heading and body alike.
    pub(crate) fn automation_hit_at(&self, row: u16, area: Rect) -> Option<AutomationEventOrigin> {
        if area.height == 0 {
            return None;
        }
        let width = self.viewport_width;
        let doc_row = self.doc_row(row, area);
        let (_, segment, start) = self.cache.segment_at_row(doc_row, width)?;
        let rel = u16::try_from(doc_row - start).ok()?;
        if rel < segment.chrome(width).margin_top {
            return None;
        }
        match &self.messages.get(segment.msg_index?)?.role {
            DisplayRole::AutomationDelivery(origin) => Some(origin.as_ref().clone()),
            _ => None,
        }
    }

    pub(crate) fn task_prompt(&self, call_id: &str) -> Option<String> {
        let (parent, child) = batch_child_id(call_id)
            .map_or((call_id, None), |(parent, child)| (parent, Some(child)));
        let message = self
            .messages
            .iter()
            .rfind(|msg| msg.role.tool_id() == Some(parent))?;
        let input = match child {
            Some(index) => {
                let entry = match message.tool_output.as_deref() {
                    Some(ToolOutput::Batch { entries, .. }) => entries
                        .get(index)
                        .and_then(|entry| entry.raw_input.as_ref()),
                    _ => None,
                };
                entry.or_else(|| {
                    let call = message
                        .tool_raw_input
                        .as_deref()?
                        .get("tool_calls")?
                        .get(index)?;
                    Some(call.get("parameters").unwrap_or(call))
                })?
            }
            None => message.tool_raw_input.as_deref()?,
        };
        input.get("prompt")?.as_str().map(str::to_owned)
    }

    pub fn workflow_card_update(&mut self, run: &RunSnapshot) -> bool {
        let card = WorkflowRunCard::from(run);
        let slash_id = workflow_card::card_id(&run.run_id);
        let Some(msg) = self.messages.iter_mut().rfind(|msg| match &msg.role {
            DisplayRole::Tool(tool) => {
                tool.id == slash_id
                    || matches!(
                        msg.tool_output.as_deref(),
                        Some(ToolOutput::WorkflowRun(existing)) if existing.run_id == run.run_id
                    )
            }
            _ => false,
        }) else {
            return false;
        };
        let DisplayRole::Tool(tool) = &mut msg.role else {
            return false;
        };
        tool.status = workflow_card::status(run.status);
        let tool_id = tool.id.clone();
        msg.annotation = Some(workflow_card::annotation(&card));
        msg.tool_output = Some(Arc::new(ToolOutput::WorkflowRun(Box::new(card))));
        self.rebuild_tool_segment(&tool_id);
        true
    }

    /// What a click at `row` on a workflow card names: its scratch file when
    /// the row is the line that lists one, else the run itself.
    pub(crate) fn workflow_hit_at(&self, row: u16, area: Rect) -> Option<CardHit> {
        if area.height == 0 {
            return None;
        }
        let width = self.viewport_width;
        let doc_row = self.doc_row(row, area);
        let (_, segment, start) = self.cache.segment_at_row(doc_row, width)?;
        let rel = u16::try_from(doc_row - start).ok()?;
        if rel < segment.chrome(width).margin_top {
            return None;
        }
        let tool_id = segment.tool_id.as_deref()?;
        let card = self
            .messages
            .iter()
            .rfind(|msg| matches!(&msg.role, DisplayRole::Tool(tool) if tool.id == tool_id))
            .and_then(|msg| match msg.tool_output.as_deref() {
                Some(ToolOutput::WorkflowRun(card)) => Some(card),
                _ => None,
            })?;
        match (&card.scratch_path, segment.row_target_at(rel, width)) {
            (Some(path), Some(_)) => Some(CardHit::ScratchFile(PathBuf::from(path))),
            _ => Some(CardHit::Run(card.run_id.clone())),
        }
    }

    /// The note a click at `row` on a memory card names, when the row is one of
    /// a note held on this host. A row answering for nothing leaves the click
    /// to the card's own control.
    pub(crate) fn memory_hit_at(&self, row: u16, area: Rect) -> Option<PathBuf> {
        if area.height == 0 {
            return None;
        }
        let width = self.viewport_width;
        let doc_row = self.doc_row(row, area);
        let (_, segment, start) = self.cache.segment_at_row(doc_row, width)?;
        let rel = u16::try_from(doc_row - start).ok()?;
        if rel < segment.chrome(width).margin_top {
            return None;
        }
        let tool_id = segment.tool_id.as_deref()?;
        let output = self
            .messages
            .iter()
            .rfind(|msg| matches!(&msg.role, DisplayRole::Tool(tool) if tool.id == tool_id))
            .and_then(|msg| match msg.tool_output.as_deref() {
                Some(ToolOutput::Memory(output)) => Some(output),
                _ => None,
            })?;
        memory_card::note_path(output, segment.row_target_at(rel, width)?)
    }

    /// A dispatched batch child reporting in. Its id is the batch's own with
    /// an index appended, so the envelope names a header that does not exist
    /// and the report has to be addressed to the roster instead.
    pub fn set_batch_child_progress(
        &mut self,
        tool_id: &str,
        index: usize,
        report: SubagentProgress,
    ) -> bool {
        if !self.batch_child_running(tool_id, index)
            && !self
                .task_cards
                .get(&format!("{tool_id}:{index}"))
                .is_some_and(|task| task.active())
        {
            return false;
        }
        let children = Arc::make_mut(
            self.batch_child_progress
                .entry(tool_id.to_owned())
                .or_default(),
        );
        if let Some(progress) = children.get_mut(&index) {
            progress.update(report);
        } else {
            children.insert(index, ToolProgress::live(report));
        }
        self.mark_card_dirty(tool_id);
        true
    }

    /// A dispatched batch child streaming its output. Its id is the batch's
    /// own with an index appended, so the envelope names a header that does
    /// not exist and the tail has to be addressed to the roster instead.
    ///
    /// Kept whole, as a standalone card keeps its live output: the window the
    /// child is drawn in decides how much of it shows, and the sink upstream
    /// already bounds what a command that prints forever can send.
    pub fn set_batch_child_output(&mut self, tool_id: &str, index: usize, content: &str) -> bool {
        if !self.batch_child_running(tool_id, index) {
            return false;
        }
        Arc::make_mut(
            self.batch_child_output
                .entry(tool_id.to_owned())
                .or_default(),
        )
        .insert(index, content.to_owned());
        if let Some(root) = self.root_tool_id(tool_id) {
            self.mark_card_dirty(root);
        }
        true
    }

    /// What a dispatched child has streamed so far, which is the only place
    /// its output is held: it has no header of its own to carry it.
    pub fn batch_child_stream(&self, tool_id: &str, index: usize) -> Option<&str> {
        Some(self.batch_child_output.get(tool_id)?.get(&index)?.as_str())
    }

    /// What a child streamed is superseded by what it returned, so the tail is
    /// dropped the moment its own result can be drawn.
    fn forget_child_output(&mut self, tool_id: &str, index: usize) {
        self.live_bufs.remove(&format!("{tool_id}:{index}"));
        if let Some(children) = self.batch_child_output.get_mut(tool_id)
            && Arc::make_mut(children).remove(&index).is_some()
            && children.is_empty()
        {
            self.batch_child_output.remove(tool_id);
        }
    }

    /// Puts a clock on every shell child that is running without one, and
    /// takes it off every child that is not.
    ///
    /// Reconciled against the roster rather than stamped on the transition,
    /// because a roster can arrive with a child already running — a start
    /// re-delivered for a batch mid-flight does exactly that — and a clock
    /// that is never started never counts. Leaving the map exactly the set of
    /// running shell children is also what lets `refresh_live_progress` ask
    /// whether it is empty instead of re-deriving the condition.
    fn sync_child_clocks(&mut self, tool_id: &str) {
        let ticking: Vec<usize> = match self
            .tool_card(tool_id)
            .and_then(|(index, _)| self.messages[index].tool_output.as_deref())
        {
            Some(ToolOutput::Batch { entries, .. }) => entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    entry.status == BatchToolStatus::Running
                        && names_tool(SHELL_TOOL_NAME, &entry.tool)
                })
                .map(|(index, _)| index)
                .collect(),
            _ => return,
        };
        if ticking.is_empty() {
            self.batch_child_started.remove(tool_id);
            return;
        }
        let children = Arc::make_mut(
            self.batch_child_started
                .entry(tool_id.to_owned())
                .or_default(),
        );
        children.retain(|index, _| ticking.contains(index));
        for index in ticking {
            children.entry(index).or_insert_with(Instant::now);
        }
    }

    fn batch_child_running(&self, tool_id: &str, index: usize) -> bool {
        if self.active_shell_call(&format!("{tool_id}:{index}")) {
            return true;
        }
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
            .filter(|msg| Self::is_running(msg))
            .and_then(|msg| match msg.tool_output.as_deref() {
                Some(ToolOutput::Batch { entries, .. }) => entries.get(index),
                _ => None,
            })
            .is_some_and(|entry| !entry.status.is_terminal())
    }

    fn settle_child_progress(&mut self, tool_id: &str, index: usize) {
        if self
            .task_cards
            .get(&format!("{tool_id}:{index}"))
            .is_some_and(|task| task.active())
        {
            return;
        }
        if let Some(scroll) = self.card_scroll.get_mut(&child_scroll_id(tool_id, index)) {
            scroll.history_base = None;
        }
        if let Some(children) = self.batch_child_progress.get_mut(tool_id)
            && let Some(progress) = Arc::make_mut(children).get_mut(&index)
        {
            progress.settle();
        }
    }

    fn settle_batch_snapshot(&mut self, tool_id: &str) {
        let Some(msg) = self.find_tool_msg_mut(tool_id) else {
            return;
        };
        let Some(ToolOutput::Batch { entries, .. }) = msg.tool_output.as_deref() else {
            return;
        };
        let terminal: Vec<_> = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.status.is_terminal().then_some(index))
            .collect();
        for index in terminal {
            self.settle_child_progress(tool_id, index);
            self.forget_child_output(tool_id, index);
        }
    }

    pub fn tool_output(&mut self, tool_id: &str, content: &str) {
        if self.tool_card(tool_id).is_none() {
            if let Some((parent, index)) = batch_child_id(tool_id) {
                self.set_batch_child_output(parent, index, content);
            }
            return;
        }
        if !self.tool_in_progress(tool_id) && !self.active_shell_call(tool_id) {
            return;
        }
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let tool_name = msg.role.tool_name().unwrap_or("");
        truncate_to_header(&mut msg.text);
        let output_lines = if tool_name == "shell" {
            SHELL_LIVE_OUTPUT_LINES
        } else {
            self.tool_output_lines.get(tool_name)
        };
        let truncated = if tool_name == "shell" {
            truncate_output_tail(content, output_lines)
        } else {
            truncate_output(content, output_lines)
        };
        msg.truncated_lines = truncated.skipped;
        msg.text.push('\n');
        msg.text.push_str(&truncated.kept);
        msg.live_output = Some(content.to_owned());
        self.mark_card_dirty(tool_id);
    }

    pub fn tool_done(&mut self, event: ToolDoneEvent) {
        self.clear_history_anchors(&event.id);
        self.dirty_cards.remove(&event.id);
        let active_shell = |call_id: &str| {
            self.task_cards
                .get(call_id)
                .or_else(|| task_card::find_call(&event.output, call_id))
                .is_some_and(|task| task.kind == JobKind::Shell && task.active())
        };
        let keep_live = active_shell(&event.id);
        let retained_children: HashSet<_> = self
            .live_bufs
            .keys()
            .filter(|call_id| active_shell(call_id))
            .cloned()
            .collect();
        let child_prefix = format!("{}:", event.id);
        self.live_bufs
            .retain(|id, _| !id.starts_with(&child_prefix) || retained_children.contains(id));
        let retain_live_output = keep_live || matches!(&event.output, ToolOutput::Shell(_));
        let had_live_buf = if keep_live {
            self.live_bufs.contains_key(&event.id)
        } else {
            self.retire_live_buf(&event.id)
        };
        if retain_live_output && !keep_live {
            self.stop_watching(&event.id);
        }
        // A child whose own terminal event never arrived stops here with the
        // batch, so no row is left counting against a clock that has stopped.
        if let Some(children) = self.batch_child_progress.get_mut(&event.id) {
            Arc::make_mut(children)
                .iter_mut()
                .filter(|(index, _)| {
                    !self
                        .task_cards
                        .get(&format!("{}:{index}", event.id))
                        .is_some_and(|task| task.active())
                })
                .for_each(|(_, progress)| progress.settle());
        }
        if let Some(children) = self.batch_child_output.get_mut(&event.id) {
            Arc::make_mut(children).retain(|index, _| {
                task_card::find_call(&event.output, &format!("{}:{index}", event.id))
                    .is_some_and(|task| task.kind == JobKind::Shell && task.active())
            });
            if children.is_empty() {
                self.batch_child_output.remove(&event.id);
            }
        }
        self.batch_child_started.remove(&event.id);
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == event.id))
        else {
            return;
        };
        if let DisplayRole::Tool(t) = &mut msg.role {
            t.status = if event.is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Success
            };
        }
        if let Some(progress) = &mut msg.progress
            && !self
                .task_cards
                .get(&event.id)
                .is_some_and(|task| task.active())
        {
            progress.settle();
        }
        msg.live_body = None;
        msg.tool_preview_pending = false;
        msg.tool_stage = None;
        if retain_live_output && !keep_live {
            msg.render_snapshot = None;
        }
        truncate_to_header(&mut msg.text);
        let done_annotation = event
            .annotation
            .as_deref()
            .map(str::to_owned)
            .or_else(|| event.output.annotation());
        if let Some(suffix) = &done_annotation {
            append_annotation(&mut msg.annotation, suffix);
        }

        match &event.output {
            ToolOutput::Plain(text) | ToolOutput::Markdown(text) | ToolOutput::ReadDir(text)
                if msg.render_snapshot.is_none() =>
            {
                if had_live_buf {
                    // The plugin streamed a body buf but no snapshot ever
                    // landed: this is the raw llm_output glitch users report.
                    warn!(
                        tool_id = %event.id,
                        tool = %event.tool,
                        is_error = event.is_error,
                        "live buf had no snapshot at tool_done; falling back to llm_output"
                    );
                }
                let tr = truncate_output(&text.text, self.tool_output_lines.get(&event.tool));
                msg.truncated_lines = tr.skipped;
                if !tr.kept.is_empty() {
                    msg.text = format!("{}\n{}", msg.text, tr.kept);
                }
            }
            ToolOutput::GrepResult { entries, .. } if entries.is_empty() => {
                msg.text = format!("{}\n{NO_FILES_FOUND}", msg.text);
            }
            _ => {}
        }
        msg.tool_output = Some(Arc::new(event.output));
        let retained_live_output = retain_live_output && msg.live_output.is_some();
        if !retain_live_output {
            msg.live_output = None;
        }
        let evicted = if retained_live_output {
            self.retained_shell_outputs.push_back(event.id.clone());
            (self.retained_shell_outputs.len() > WARM_TOOL_CAP)
                .then(|| self.retained_shell_outputs.pop_front())
                .flatten()
        } else {
            None
        };
        self.rebuild_tool_segment(&event.id);
        if let Some(evicted) = evicted {
            if let Some(message) = self.messages.iter_mut().rfind(
                |message| matches!(&message.role, DisplayRole::Tool(tool) if tool.id == evicted),
            ) {
                message.live_output = None;
            }
            self.rebuild_tool_segment(&evicted);
        }
    }

    pub fn update_tool_summary(&mut self, tool_id: &str, summary: &str) {
        self.update_tool(tool_id, |msg| msg.text = summary.to_owned());
    }

    pub fn update_tool_model(&mut self, tool_id: &str, model: &str) {
        self.update_tool(tool_id, |msg| append_annotation(&mut msg.annotation, model));
    }

    pub fn set_tool_progress(&mut self, tool_id: &str, report: SubagentProgress) {
        if !self.tool_in_progress(tool_id)
            && !self
                .task_cards
                .get(tool_id)
                .is_some_and(|task| task.active())
        {
            return;
        }
        self.update_tool(tool_id, |msg| {
            if let Some(progress) = &mut msg.progress {
                progress.update(report);
            } else {
                msg.progress = Some(ToolProgress::live(report));
            }
        });
    }

    pub fn tool_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.store_snapshot(tool_id, snapshot, false, theme_gen);
    }

    pub fn tool_annotation(&mut self, tool_id: &str, annotation: String) {
        if self.tool_card(tool_id).is_some()
            && (self.tool_in_progress(tool_id) || self.active_shell_call(tool_id))
        {
            self.update_tool(tool_id, |msg| msg.annotation = Some(annotation));
        } else if let Some((parent, index)) = batch_child_id(tool_id)
            && self.batch_child_running(parent, index)
            && let Some(root) = self.root_tool_id(tool_id)
            && let Some(indices) = tool_id
                .strip_prefix(root)
                .and_then(|suffix| suffix.strip_prefix(':'))
            && let Some(msg) = self.find_tool_msg_mut(root)
            && let Some(output) = &mut msg.tool_output
            && let Some(entry) = batch_entry_mut(Arc::make_mut(output), indices)
        {
            entry.annotation = Some(annotation);
            self.mark_card_dirty(root);
        }
    }

    pub fn tool_header_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.store_snapshot(tool_id, snapshot, true, theme_gen);
    }

    /// A subagent stamps its own cumulative usage on the task header, and that
    /// header is usually the last tool of the turn, so an existing stamp wins.
    pub fn set_turn_usage_on_last_tool(&mut self, usage: String) {
        let last_tool = self.messages.iter().rev().find_map(|msg| match &msg.role {
            DisplayRole::Tool(tool) => Some((tool.id.clone(), msg.turn_usage.is_none())),
            _ => None,
        });
        if let Some((id, unstamped)) = last_tool
            && unstamped
        {
            self.set_tool_turn_usage(&id, usage);
        }
    }

    pub fn set_tool_turn_usage(&mut self, tool_id: &str, usage: String) {
        self.update_tool(tool_id, |msg| msg.turn_usage = Some(usage));
    }

    fn upsert_instruction_segment(
        &mut self,
        parent_id: &str,
        blocks: &[InstructionBlock],
        parent_idx: usize,
    ) {
        if blocks.is_empty() {
            return;
        }
        let inst_id = segment::instruction_id(parent_id);
        let exp = self
            .tool_expansion(&inst_id, self.card_opens_by_default(parent_id))
            .map(|d| d.full);
        let width = SegmentChrome::for_kind(SegmentKind::Instruction, self.viewport_width, 0)
            .content_width(self.viewport_width);
        let tl = build_instructions_lines(blocks, width, exp);

        if let Some(seg_idx) = self.cache.find_by_tool_id(&inst_id) {
            let compact = self.card_draws_compact(parent_id);
            let seg = self.cache.get_mut(seg_idx).unwrap();
            seg.search_text = tl.search_text.clone();
            seg.update_with_reuse(tl, &self.hl_worker, compact);
        } else {
            let compact = self.card_draws_compact(parent_id);
            let msg_index = self.cache.get(parent_idx).and_then(|s| s.msg_index);
            let mut seg = Segment::with_tool(inst_id, SegmentKind::Instruction, msg_index);
            seg.search_text = tl.search_text.clone();
            seg.apply_highlight(tl, &self.hl_worker, compact);
            self.cache.insert(parent_idx + 1, seg);
        }
        self.cache.update_margins(self.viewport_width);
    }

    fn update_tool(&mut self, tool_id: &str, update_msg: impl FnOnce(&mut DisplayMessage)) {
        let Some(msg) = self.find_tool_msg_mut(tool_id) else {
            return;
        };
        update_msg(msg);
        self.mark_card_dirty(tool_id);
    }

    pub fn stream_reset(&mut self) {
        self.streaming_thinking.clear();
        self.streaming_text.clear();
        self.streaming_role = DisplayRole::Assistant;
        self.streaming_reasoning_open = None;
        self.thinking_started = None;
        self.forget_thinking_windows(|block| block == ThinkingWindow::Live);
        self.cancel_in_progress();
    }

    pub fn fail_in_progress_with_message(&mut self, message: String) {
        self.fail_in_progress_except(message, &HashSet::new());
    }

    pub fn fail_in_progress_except(&mut self, message: String, excluded: &HashSet<String>) {
        let ids: Vec<(String, Arc<str>)> = self
            .messages
            .iter()
            .filter_map(|m| {
                if let DisplayRole::Tool(t) = &m.role
                    && t.status == ToolStatus::InProgress
                    && !excluded.contains(&t.id)
                    && !Self::is_live_workflow(m)
                {
                    Some((t.id.clone(), Arc::clone(&t.name)))
                } else {
                    None
                }
            })
            .collect();
        for (id, tool) in ids {
            self.tool_done(ToolDoneEvent {
                id,
                tool,
                output: ToolOutput::Plain(message.clone().into()),
                is_error: true,
                annotation: None,
                written_path: None,
                written_paths: Vec::new(),
                remote_written_paths: false,
                output_ref: None,
                output_limits: None,
                model_suffix: None,
                model_output: None,
                model_output_from_ref: false,
                documents: Vec::new(),
                accounting: caudra_agent::ToolAccounting::default(),
            });
        }
    }

    pub fn cancel_in_progress(&mut self) {
        let affected_ids: Vec<String> = self
            .messages
            .iter_mut()
            .filter_map(|msg| {
                if Self::is_live_workflow(msg) {
                    return None;
                }
                if let DisplayRole::Tool(t) = &mut msg.role
                    && t.status == ToolStatus::InProgress
                {
                    t.status = ToolStatus::Error;
                    Some(t.id.clone())
                } else {
                    None
                }
            })
            .collect();

        for id in &affected_ids {
            self.clear_history_anchors(id);
            // The stale-run_id filter drops these tools' ToolDone events,
            // so retire their live bufs here: keeps them clickable via
            // the warm path and stops them being polled forever.
            self.retire_live_buf(id);
            self.remove_child_live_bufs(id);
            self.batch_child_output.remove(id);
            self.batch_child_started.remove(id);
            self.dirty_cards.remove(id);
            self.card_floor.remove(id);
            if let Some(children) = self.batch_child_progress.get_mut(id) {
                Arc::make_mut(children)
                    .values_mut()
                    .for_each(ToolProgress::settle);
            }
            if let Some(msg) = self.find_tool_msg_mut(id) {
                msg.tool_preview_pending = false;
                msg.tool_stage = None;
                msg.live_body = None;
                if let Some(output) = &mut msg.tool_output
                    && let ToolOutput::Batch { entries, .. } = Arc::make_mut(output)
                {
                    // A child that was live is cut off. One still being
                    // written joins the others that never went out.
                    for entry in entries {
                        entry.status = match entry.status {
                            BatchToolStatus::AwaitingApproval | BatchToolStatus::Running => {
                                BatchToolStatus::Error
                            }
                            BatchToolStatus::Drafting => BatchToolStatus::Pending,
                            BatchToolStatus::Pending
                            | BatchToolStatus::Success
                            | BatchToolStatus::Error => entry.status,
                        };
                    }
                }
                if let Some(progress) = &mut msg.progress {
                    progress.settle();
                }
            }
            self.rebuild_tool_segment(id);
        }
    }

    /// Only tests care how many; the panel itself only ever asks whether.
    #[cfg(test)]
    pub fn in_progress_count(&self) -> usize {
        self.messages.iter().filter(|m| Self::is_running(m)).count()
    }

    /// Searched from the tail because a running call is the newest thing in
    /// the transcript, so this settles in a handful of steps where counting
    /// walks every message. It runs once per loop turn from `cadence`, which
    /// is far more often than a frame.
    fn has_in_progress(&self) -> bool {
        self.messages.iter().rev().any(Self::is_running)
            || self.task_cards.values().any(TaskCard::active)
    }

    fn is_running(msg: &DisplayMessage) -> bool {
        matches!(&msg.role, DisplayRole::Tool(t) if t.status == ToolStatus::InProgress)
    }

    /// A workflow card is a live view of a run that outlives the turn that
    /// started it, not a call waiting to report. The sweeps that end a turn
    /// leave it to the runtime, which keeps repainting it from snapshots.
    fn is_live_workflow(msg: &DisplayMessage) -> bool {
        matches!(msg.tool_output.as_deref(), Some(ToolOutput::WorkflowRun(_)))
    }

    #[cfg(test)]
    pub fn toggle_expansion(&mut self, tool_id: &str) -> bool {
        let Some(seg) = self
            .cache
            .segments()
            .iter()
            .find(|s| s.tool_id.as_deref() == Some(tool_id))
        else {
            return false;
        };
        let full = self.disclosure.get(tool_id) == Some(&CardState::Full);
        if !seg.truncation && !full {
            return false;
        }
        let tool_id = tool_id.to_owned();
        if full {
            self.rest_card(&tool_id);
        } else {
            self.open_card(&tool_id);
        }
        true
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    #[cfg(test)]
    pub fn message_at(&self, index: usize) -> Option<&DisplayMessage> {
        self.messages.get(index)
    }

    #[cfg(test)]
    pub fn last_message_text(&self) -> &str {
        self.messages.last().map(|m| m.text.as_str()).unwrap_or("")
    }

    #[cfg(test)]
    pub fn last_message_is_plan(&self) -> bool {
        self.messages.last().is_some_and(|m| m.plan_path.is_some())
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages.last().map(|m| &m.role)
    }

    #[cfg(test)]
    pub fn rebake_requested_gen(&self, tool_id: &str) -> Option<u64> {
        self.rebake_requested.get(tool_id).copied()
    }

    #[cfg(test)]
    pub fn snapshot_gen_of(&self, tool_id: &str) -> Option<u64> {
        self.current_snapshot_gen(tool_id)
    }

    #[cfg(test)]
    pub fn streaming_text_is_empty(&self) -> bool {
        self.streaming_text.is_empty()
    }

    #[cfg(test)]
    pub fn streaming_thinking_is_empty(&self) -> bool {
        self.streaming_thinking.is_empty()
    }

    #[cfg(test)]
    pub fn tool_turn_usage(&self, tool_id: &str) -> Option<&str> {
        self.messages.iter().rev().find_map(|msg| match &msg.role {
            DisplayRole::Tool(tool) if tool.id == tool_id => msg.turn_usage.as_deref(),
            _ => None,
        })
    }

    pub fn set_prompt_progress(&mut self, progress: Option<PromptProgress>) {
        match progress {
            Some(progress) => self.prompt_rate.sample(progress.processed, Instant::now()),
            None => self.prompt_rate.reset(),
        }
        self.prompt_progress = progress;
    }

    pub fn clear_prompt_progress(&mut self) {
        self.prompt_progress = None;
        self.prompt_rate.reset();
    }

    pub fn flush(&mut self) {
        self.flush_thinking();
        self.prompt_progress = None;
        self.prompt_rate.reset();
        if !self.streaming_text.is_empty() {
            self.messages.push(DisplayMessage::new(
                self.streaming_role.clone(),
                self.streaming_text.take_all(),
            ));
        }
        self.streaming_role = DisplayRole::Assistant;
    }

    pub fn scroll(&mut self, delta: i32) {
        self.set_scroll_top(apply_scroll_rows(self.scroll_top, delta));
    }

    /// Following is decided here, from the move the reader just made, against
    /// the document they were looking at when they made it. Leaving it to the
    /// next frame's clamp cannot tell the two apart: a card above getting
    /// shorter is not the reader asking to follow, and while cards stream
    /// that happens constantly.
    ///
    /// A document with no scrollback is settled by the frame instead, since a
    /// move inside one is neither a pause nor a resume; see
    /// [`Self::resolve_scroll`].
    pub fn set_scroll_top(&mut self, top: u32) {
        self.clear_hover();
        let bottom = self.max_scroll();
        self.scroll_top = top.min(bottom);
        self.auto_scroll = bottom > 0 && top >= bottom;
        self.hold_reading_position();
    }

    pub fn auto_scroll(&self) -> bool {
        self.auto_scroll
    }

    pub fn scroll_to_top(&mut self) {
        self.set_scroll_top(0);
    }

    pub fn enable_auto_scroll(&mut self) {
        self.clear_hover();
        self.auto_scroll = true;
        self.hold_reading_position();
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        let width = self.viewport_width;
        let offset = self
            .cache
            .segments()
            .iter()
            .take(segment_index)
            .map(|s| s.height(width) as u32)
            .sum::<u32>();
        self.set_scroll_top(offset);
    }

    pub fn restore_scroll(&mut self, scroll_top: u32, auto_scroll: bool) {
        self.clear_hover();
        self.scroll_top = scroll_top;
        self.auto_scroll = auto_scroll;
        self.hold_reading_position();
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.highlight_segment = idx;
    }

    pub fn half_page(&self) -> i32 {
        self.viewport_height as i32 / 2
    }

    pub fn set_accent(&mut self, color: ratatui::style::Color) {
        self.accent.set(color);
    }

    pub fn tool_id_at(&self, row: u16, area: Rect) -> Option<&str> {
        if area.height == 0 {
            return None;
        }
        let doc_row = self.doc_row(row, area);
        let (_, segment, start) = self.cache.segment_at_row(doc_row, self.viewport_width)?;
        let rel = u16::try_from(doc_row - start).ok()?;
        (rel >= segment.chrome(self.viewport_width).margin_top)
            .then_some(segment.tool_id.as_deref())
            .flatten()
    }

    /// What produced the row under the pointer: the segment, the message
    /// behind it, and the call it belongs to. A grab's component stack names
    /// the code that drew a cell, and this names the data it drew.
    #[cfg(debug_assertions)]
    pub fn grab_provenance_at(&self, row: u16, area: Rect) -> Option<String> {
        if area.height == 0 {
            return None;
        }
        let doc_row = self.doc_row(row, area);
        let (index, segment, _) = self.cache.segment_at_row(doc_row, self.viewport_width)?;
        let mut parts = vec![format!("segment {index}")];
        if let Some(msg_index) = segment.msg_index {
            let role = self
                .messages
                .get(msg_index)
                .map(|message| format!(" ({:?})", message.role))
                .unwrap_or_default();
            parts.push(format!("message {msg_index}{role}"));
        }
        if let Some(tool_id) = segment.tool_id.as_deref() {
            parts.push(format!("tool {tool_id}"));
        }
        Some(parts.join(", "))
    }

    /// The dispatched id of the batch child at `row`, which is the card's own
    /// with the child's index appended. Rebuilding it is what lets a click on
    /// a roster row reach the subagent that row dispatched, since the roster
    /// carries no ids of its own.
    pub fn dispatched_id_at(&self, row: u16, area: Rect) -> Option<String> {
        if area.height == 0 {
            return None;
        }
        let doc_row = self.doc_row(row, area);
        let (_, segment, start) = self.cache.segment_at_row(doc_row, self.viewport_width)?;
        let rel = u16::try_from(doc_row - start).ok()?;
        let tool_id = segment.tool_id.as_deref()?;
        let index = segment.row_target_at(rel, self.viewport_width)?.index();
        Some(format!("{tool_id}:{index}"))
    }

    pub fn source_at(&self, row: u16, area: Rect) -> Option<DisplaySource> {
        if area.height == 0 || row < area.y || row >= area.bottom() {
            return None;
        }
        let doc_row = self.doc_row(row, area);
        let (_, segment, start) = self.cache.segment_at_row(doc_row, self.viewport_width)?;
        let rel = u16::try_from(doc_row - start).ok()?;
        if rel < segment.chrome(self.viewport_width).margin_top {
            return None;
        }
        self.segment_source(segment)
    }

    fn segment_source(&self, segment: &Segment) -> Option<DisplaySource> {
        if let Some(index) = segment.msg_index {
            return self.messages.get(index)?.source;
        }
        let tool_id = segment
            .tool_id
            .as_deref()
            .and_then(segment::instruction_parent)
            .or(segment.tool_id.as_deref())?;
        // `rfind`, not `find_map`: a card whose message carries no source is
        // the answer, not a reason to keep walking to the front of the
        // transcript.
        self.messages
            .iter()
            .rfind(|message| matches!(&message.role, DisplayRole::Tool(tool) if tool.id == tool_id))
            .and_then(|message| message.source)
    }

    fn message_action_source(&self, segment: &Segment) -> Option<DisplaySource> {
        if segment
            .tool_id
            .as_deref()
            .is_some_and(segment::is_instruction_segment)
        {
            return None;
        }
        self.segment_source(segment)
    }

    pub(crate) fn update_hover(
        &mut self,
        row: u16,
        col: u16,
        area: Rect,
        known_task_target: bool,
        cwd: &Path,
    ) {
        self.hover = self.hover_target_at(row, col, area, known_task_target, cwd, false);
        self.disarm_unless_over(col, row);
    }

    pub(crate) fn update_hover_remote(
        &mut self,
        row: u16,
        col: u16,
        area: Rect,
        known_task_target: bool,
    ) {
        self.hover = self.hover_target_at(row, col, area, known_task_target, Path::new(""), true);
        self.disarm_unless_over(col, row);
    }

    pub(crate) fn message_action_at(&self, row: u16, col: u16) -> Option<MessageActionTarget> {
        let position = Position::new(col, row);
        self.message_action_hits
            .iter()
            .find(|hit| hit.area.contains(position))
            .map(|hit| hit.target)
    }

    pub(crate) fn clear_hover(&mut self) {
        self.hover = None;
    }

    /// Releases the armed window. Kept apart from `clear_hover` on purpose:
    /// hover is transient feedback that many paths clear freely, and arming is
    /// a gesture the reader performed that has to outlive them.
    pub(crate) fn disarm_card(&mut self) {
        self.armed_card = None;
    }

    /// What the status bar says about whatever the pointer is over. Neither
    /// target marks its own glyphs, so this line is the only sign either of
    /// them is there.
    pub(crate) fn hovered_hint(&self) -> Option<&str> {
        match &self.hover {
            Some(HoverTarget::Link(target)) => Some(target),
            Some(HoverTarget::Mention(mention)) => Some(&mention.raw),
            // A hash names nothing on its own, so the bar shows the subject the
            // reader would otherwise have to open the commit to read.
            Some(HoverTarget::Commit(commit)) => {
                Some(self.commit_index.subject(&commit.id).unwrap_or(&commit.raw))
            }
            _ => None,
        }
    }

    pub(crate) fn terminal_links(&self) -> &[TerminalLink] {
        &self.terminal_links
    }

    pub(crate) fn link_at(&self, row: u16, col: u16, area: Rect) -> Option<Arc<str>> {
        if area.height == 0
            || row < area.y
            || row >= area.bottom()
            || col < area.x
            || col >= area.right()
        {
            return None;
        }
        let width = self.viewport_width;
        let doc_row = self.doc_row(row, area);
        let rel_col = col - area.x;
        if let Some((_, segment, start)) = self.cache.segment_at_row(doc_row, width) {
            let rel_row = u16::try_from(doc_row - start).ok()?;
            return segment.link_at(rel_row, rel_col, width);
        }
        self.streaming_link_at(doc_row, rel_col, width)
    }

    /// The mention under the pointer, resolved against the markdown the message
    /// was painted from rather than the glyphs on screen, so a mention still
    /// names the file the reader typed wherever the renderer reworded the line
    /// around it.
    ///
    /// Only a user message answers. A mention is something the reader wrote,
    /// and a path the model happens to spell with an `@` was never a request to
    /// open anything.
    pub(crate) fn mention_at(&self, row: u16, col: u16, area: Rect, cwd: &Path) -> Option<Mention> {
        self.mention_at_mode(row, col, area, cwd, false)
    }

    pub(crate) fn mention_at_remote(&self, row: u16, col: u16, area: Rect) -> Option<Mention> {
        self.mention_at_mode(row, col, area, Path::new(""), true)
    }

    /// The commit a sent `#hash` names, under the pointer. Only the reader's
    /// own messages answer, for the same reason mentions do: a hash the model
    /// happens to write was never a request to open anything.
    pub(crate) fn commit_at(&self, row: u16, col: u16, area: Rect) -> Option<CommitRef> {
        let (source, offset) = self.source_under(row, col, area)?;
        commits::scan(&source, |id| self.commit_index.resolves(id))
            .into_iter()
            .find(|(range, _)| range.contains(&offset))
            .map(|(_, commit)| commit)
    }

    pub(crate) fn set_commit_index(&mut self, index: CommitIndex) {
        self.commit_index = index;
    }

    /// Nothing is redrawn: the directory only changes while no call is
    /// running, and a call that ran names the directory its result recorded.
    pub(crate) fn set_cwd(&mut self, cwd: &Path) {
        self.cwd = Some(Arc::from(cwd));
    }

    /// The text of the reader's own message under the pointer, and how far into
    /// it the pointer is. Shared by everything that resolves a token in the
    /// transcript, so none of them can disagree about what is clickable.
    fn source_under(&self, row: u16, col: u16, area: Rect) -> Option<(Arc<str>, usize)> {
        if area.height == 0
            || row < area.y
            || row >= area.bottom()
            || col < area.x
            || col >= area.right()
        {
            return None;
        }
        let width = self.viewport_width;
        let doc_row = self.doc_row(row, area);
        let (_, segment, start) = self.cache.segment_at_row(doc_row, width)?;
        if segment.kind() != SegmentKind::User {
            return None;
        }
        let rel_row = u16::try_from(doc_row - start).ok()?;
        let (source, byte) = segment.source_at(rel_row, col - area.x, width)?;
        // Provenance counts bytes and the scanner counts chars.
        let offset = source.get(..byte as usize)?.chars().count();
        Some((source, offset))
    }

    fn mention_at_mode(
        &self,
        row: u16,
        col: u16,
        area: Rect,
        cwd: &Path,
        remote: bool,
    ) -> Option<Mention> {
        let (source, offset) = self.source_under(row, col, area)?;
        let mentions = if remote {
            mentions::scan_remote(&source)
        } else {
            mentions::scan_in(&source, cwd)
        };
        mentions
            .into_iter()
            .find(|(range, _)| range.contains(&offset))
            .map(|(_, mention)| mention)
    }

    fn hover_target_at(
        &self,
        row: u16,
        col: u16,
        area: Rect,
        known_task_target: bool,
        cwd: &Path,
        remote: bool,
    ) -> Option<HoverTarget> {
        if let Some(target) = self.message_action_at(row, col) {
            return Some(HoverTarget::MessageAction(target.segment_index));
        }
        if area.height == 0
            || row < area.y
            || row >= area.bottom()
            || col < area.x
            || col >= area.right()
        {
            return None;
        }
        let width = self.viewport_width;
        let doc_row = self.doc_row(row, area);
        let Some((_, segment, start)) = self.cache.segment_at_row(doc_row, width) else {
            if let Some(target) = self.streaming_link_at(doc_row, col - area.x, width) {
                return Some(HoverTarget::Link(target));
            }
            return self
                .is_collapsed_streaming_thinking_row(doc_row, width)
                .then_some(HoverTarget::StreamingThinking);
        };
        let rel = u16::try_from(doc_row - start).ok()?;
        if rel < segment.chrome(width).margin_top {
            return None;
        }
        if let Some(target) = segment.link_at(rel, col - area.x, width) {
            return Some(HoverTarget::Link(target));
        }
        // After the link map, so a markdown link keeps its cell wherever the
        // two somehow overlap.
        let mention = if remote {
            self.mention_at_remote(row, col, area)
        } else {
            self.mention_at(row, col, area, cwd)
        };
        if let Some(mention) = mention {
            return Some(HoverTarget::Mention(mention));
        }
        if let Some(commit) = self.commit_at(row, col, area) {
            return Some(HoverTarget::Commit(commit));
        }
        let Some(tool_id) = segment.tool_id.as_deref() else {
            let msg_index = segment.msg_index?;
            // A diagram row wins over the thinking toggle: it is the only
            // target inside a text segment that reacts to the pointer.
            if let Some(key) = self.diagram_key_at(segment, msg_index, rel, width) {
                return Some(HoverTarget::Diagram(key));
            }
            return self
                .messages
                .get(msg_index)
                .is_some_and(|message| Self::has_foldable_body(message) && !self.body_open(message))
                .then_some(HoverTarget::CachedThinking(msg_index));
        };

        // A workflow card answers to the click's own function rather than to
        // the expand and collapse rules below, which it does not obey: a press
        // anywhere on it opens the inspector.
        if let Some(hit) = self.workflow_hit_at(row, area) {
            let feedback = match hit {
                CardHit::ScratchFile(_) => segment
                    .source_line_at(rel, width)
                    .map_or(HoverFeedback::Chrome, HoverFeedback::Row),
                CardHit::Run(_) => HoverFeedback::Chrome,
            };
            return Some(HoverTarget::Tool {
                id: tool_id.to_owned(),
                feedback,
            });
        }

        // A compact snapshot tool has not reached Lua yet, so the first click
        // is served locally and the row has to advertise itself.
        let native_toggle = (!self.has_snapshot(tool_id) || self.card_closed(tool_id))
            && self.tool_click_acts(tool_id, segment.truncation);
        let shell_toggle = segment
            .shell_toggle_line
            .is_some_and(|line| segment.source_line_at(rel, width) == Some(line));
        let batch_row = segment.control_line_at(rel, width);
        if !native_toggle && !shell_toggle && batch_row.is_none() && !known_task_target {
            return None;
        }
        let feedback = if shell_toggle {
            HoverFeedback::ShellToggle
        } else if let Some(line) = batch_row {
            HoverFeedback::Row(line)
        } else if native_toggle
            && segment.lines().iter().any(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains(EXPAND_AFFORDANCE))
            })
        {
            HoverFeedback::Affordance
        } else {
            HoverFeedback::Chrome
        };
        Some(HoverTarget::Tool {
            id: tool_id.to_owned(),
            feedback,
        })
    }

    fn hover_feedback_for_segment(&self, segment: &Segment) -> Option<HoverFeedback> {
        match &self.hover {
            Some(HoverTarget::CachedThinking(msg_index))
                if segment.msg_index == Some(*msg_index) && segment.tool_id.is_none() =>
            {
                Some(HoverFeedback::Chrome)
            }
            Some(HoverTarget::Tool { id, feedback })
                if segment.tool_id.as_deref() == Some(id.as_str()) =>
            {
                Some(*feedback)
            }
            Some(HoverTarget::CachedThinking(_))
            | Some(HoverTarget::MessageAction(_))
            | Some(HoverTarget::Link(_))
            | Some(HoverTarget::Mention(_))
            | Some(HoverTarget::Commit(_))
            | Some(HoverTarget::StreamingThinking)
            | Some(HoverTarget::Tool { .. })
            | Some(HoverTarget::Diagram(_))
            | None => None,
        }
    }

    fn streaming_link_at(&self, doc_row: u32, col: u16, width: u16) -> Option<Arc<str>> {
        let mut block_start = self.cache.total_height(width);
        let mut has_previous = self.cache.len() != 0;
        let streams = [
            (
                &self.streaming_thinking,
                self.streaming_thinking_collapsed(),
                SegmentKind::Thinking,
            ),
            (&self.streaming_text, false, self.streaming_kind()),
        ];
        for (stream, collapsed, kind) in streams {
            if stream.is_empty() {
                continue;
            }
            if has_previous {
                block_start = block_start.saturating_add(1);
            }
            let chrome = SegmentChrome::for_kind(kind, width, 0);
            let content_width = chrome.content_width(width);
            if kind == SegmentKind::Thinking && !collapsed {
                let segment = self.streaming_thinking_segment();
                let lines = segment.lines();
                let height = wrapped_line_count(lines, content_width) as u32;
                if (block_start..block_start + height).contains(&doc_row) {
                    let row = u16::try_from(doc_row - block_start).ok()?;
                    let col = col.checked_sub(chrome.left)?;
                    return segment.links().target_at(lines, content_width, row, col);
                }
                block_start = block_start.saturating_add(height);
                has_previous = true;
                continue;
            }
            let lines = if collapsed {
                None
            } else {
                Some(stream.cached_lines())
            };
            let height = lines.map_or_else(
                || wrapped_line_count(&self.build_streaming_collapsed_lines(), content_width),
                |lines| wrapped_line_count(lines, content_width),
            ) as u32;
            if !collapsed && (block_start..block_start + height).contains(&doc_row) {
                let row = u16::try_from(doc_row - block_start).ok()?;
                let col = col.checked_sub(chrome.left)?;
                return stream.link_at(content_width, row, col);
            }
            block_start = block_start.saturating_add(height);
            has_previous = true;
        }
        None
    }

    /// The diagram drawn on `rel`, a row relative to the segment. Diagram
    /// rows are pre-sliced to the viewport, so the display row maps straight
    /// onto a line index.
    fn diagram_key_at(
        &self,
        segment: &Segment,
        msg_index: usize,
        rel: u16,
        width: u16,
    ) -> Option<DiagramKey> {
        if segment.diagrams().is_empty() {
            return None;
        }
        let line = segment.source_line_at(rel, width)?;
        segment.diagram_at_line(line).map(|span| DiagramKey {
            msg_index,
            id: span.id,
        })
    }

    /// The diagram the keyboard pans: whichever shows the most rows, and the
    /// later message when two show the same. Rows resolve through the lookup
    /// the pointer uses, because a line index is not a display row once
    /// anything above the diagram wraps.
    fn most_visible_diagram(&self) -> Option<DiagramKey> {
        let width = self.viewport_width;
        let mut rows: Vec<(DiagramKey, u16)> = Vec::new();
        for offset in 0..self.viewport_height {
            let doc_row = self.scroll_top.saturating_add(u32::from(offset));
            let Some((_, segment, start)) = self.cache.segment_at_row(doc_row, width) else {
                continue;
            };
            let Some(key) = segment
                .msg_index
                .zip(u16::try_from(doc_row - start).ok())
                .and_then(|(msg_index, rel)| self.diagram_key_at(segment, msg_index, rel, width))
            else {
                continue;
            };
            match rows.iter_mut().find(|(seen, _)| *seen == key) {
                Some((_, count)) => *count += 1,
                None => rows.push((key, 1)),
            }
        }
        // A chart that already fits has nothing to show for a keypress, and
        // targeting it would strand the keys on a wide chart further up.
        rows.retain(|(key, _)| self.pan_range(*key).is_some_and(|(_, max)| max > 0));
        // Rows are collected top down, so taking the last of the equal maxima
        // is what breaks a tie toward the later message.
        rows.into_iter()
            .reduce(|best, next| if next.1 >= best.1 { next } else { best })
            .map(|(key, _)| key)
    }

    /// The segment holding a chart and how far it may travel. The ceiling is
    /// zero for a chart that already fits, which is what makes it a poor
    /// target for a key that has no pointer to disambiguate it.
    fn pan_range(&self, key: DiagramKey) -> Option<(usize, u16)> {
        let width = self.viewport_width;
        let (seg_idx, full_width) =
            self.cache
                .segments()
                .iter()
                .enumerate()
                .find_map(|(idx, segment)| {
                    if segment.msg_index != Some(key.msg_index) {
                        return None;
                    }
                    let span = segment.diagrams().iter().find(|span| span.id == key.id)?;
                    Some((idx, span.full_width))
                })?;
        let content = self
            .cache
            .get(seg_idx)
            .map_or(width, |segment| segment.content_width(width));
        Some((
            seg_idx,
            caudra_markdown::render::diagram_max_pan(full_width, content),
        ))
    }

    /// Scrolls a drawn diagram sideways, reporting whether anything moved.
    fn pan_diagram(&mut self, key: DiagramKey, delta: i32) -> bool {
        let width = self.viewport_width;
        let Some((seg_idx, ceiling)) = self.pan_range(key) else {
            return false;
        };

        let current = self.diagram_pans.get(&key).copied().unwrap_or(0);
        let next = (current as i32 + delta).clamp(0, ceiling as i32) as u16;
        if next == current {
            return false;
        }
        match next {
            0 => self.diagram_pans.remove(&key),
            _ => self.diagram_pans.insert(key, next),
        };
        self.reflow_text_segment(seg_idx, width);
        self.cache.update_margins(width);
        true
    }

    /// Pans the diagram under the pointer. Used by horizontal wheel events.
    pub(crate) fn pan_hovered_diagram(&mut self, delta: i32) -> bool {
        let Some(HoverTarget::Diagram(key)) = self.hover else {
            return false;
        };
        self.pan_diagram(key, delta)
    }

    /// Pans without a pointer, for the keyboard bindings.
    pub(crate) fn pan_visible_diagram(&mut self, delta: i32) -> bool {
        let Some(key) = self.most_visible_diagram() else {
            return false;
        };
        self.pan_diagram(key, delta)
    }

    #[cfg(test)]
    pub(crate) fn panned_diagram_count(&self) -> usize {
        self.diagram_pans.len()
    }

    /// Offers a wheel burst to the armed window under the pointer, returning
    /// the notches it did not use. Anything else keeps the whole burst, so the
    /// transcript scrolls exactly as it always has.
    ///
    /// Both conditions are needed: the press says which window the reader
    /// means, and the pointer says they still mean it. Position alone would
    /// hand a card every notch that passed over it on the way somewhere else.
    pub fn scroll_card_at(&mut self, column: u16, row: u16, delta: i32) -> i32 {
        if delta == 0 {
            return delta;
        }
        let Some(key) = self.armed_at(column, row).map(str::to_owned) else {
            return delta;
        };
        self.scroll_window(&key, delta)
    }

    pub fn handle_click(&mut self, row: u16, area: Rect) -> bool {
        if area.height == 0 {
            return false;
        }
        self.clear_hover();
        let doc_row = self.doc_row(row, area);
        let width = self.viewport_width;
        // Both fallbacks toggle thinking: a row past the cached segments
        // belongs to the still-streaming indicator, and a segment without a
        // tool_id is a finished message's text.
        let Some((_, seg, seg_start)) = self.cache.segment_at_row(doc_row, width) else {
            return self.try_follow_streaming_thinking(doc_row, width)
                || self.try_toggle_collapsed_thinking(doc_row, width);
        };
        let rel = u16::try_from(doc_row - seg_start).unwrap_or(u16::MAX);
        if rel < seg.chrome(width).margin_top {
            return false;
        }
        let Some(tool_id) = seg.tool_id.as_deref() else {
            let msg_idx = seg.msg_index;
            // A reasoning window's footer is its follow control, as a card's
            // is, so it takes the window back to the tail instead of folding.
            if let Some(msg_idx) = msg_idx
                && on_scroll_footer(seg, rel, width)
            {
                self.follow_thinking(ThinkingWindow::Settled(msg_idx));
                return true;
            }
            return self.try_toggle_cached_thinking(msg_idx, width);
        };

        // A click asks a question the row budget rarely answers, so the first
        // one on a closed card shows all of the body. Lua-rendered tools
        // included: their snapshot is already here, so the runtime stays out
        // of it until the reader asks for more.
        if self.card_closed(tool_id) {
            if !seg.truncation {
                return false;
            }
            let tool_id = tool_id.to_owned();
            self.open_card(&tool_id);
            return true;
        }

        if self.has_snapshot(tool_id) {
            // Without a runtime the snapshot is passive history: a click
            // would record replays nothing can ever answer.
            if self.lua_event_handle.is_disconnected() {
                return false;
            }
            let buf_row = seg.source_line_at(rel, width).map_or(0, |l| seg.buf_row(l));
            if self.tool_in_progress(tool_id) {
                self.lua_event_handle
                    .request_click(tool_id.to_owned(), buf_row);
                return true;
            }
            // Recorded even when the warm path serves the click: theme
            // rebake and session restore replay the full sequence.
            self.lua_clicks
                .entry(tool_id.to_owned())
                .or_default()
                .push(buf_row);
            let item = self.lua_restore_item(tool_id).map(|mut item| {
                item.clicks = self.lua_clicks[tool_id].clone();
                item
            });
            let Some(tx) = self.restore_event_tx.clone() else {
                return true;
            };
            let eh = &self.lua_event_handle;
            // Watching the buf means a runtime-side warm click would be
            // visible here, so try the fast path; the fallback item lets
            // the runtime degrade to restore+replay if its cache is cold.
            // Without the buf only a fresh restore can show the result.
            match (self.watching(tool_id), item) {
                (true, Some(item)) => {
                    eh.request_click_with_fallback(tool_id.to_owned(), buf_row, item, tx);
                }
                (true, None) => eh.request_click(tool_id.to_owned(), buf_row),
                (false, Some(item)) => eh.request_restore(item, tx),
                (false, None) => {}
            }
            return true;
        }

        let exp = self
            .tool_expansion(tool_id, self.card_opens_by_default(tool_id))
            .unwrap_or_default();
        let source_line = seg.source_line_at(rel, width);
        // The footer is what says the window is paused, so it is also what
        // takes it back to the tail.
        if seg
            .scroll_footer_line
            .is_some_and(|line| source_line == Some(line))
        {
            let tool_id = tool_id.to_owned();
            self.follow_card(&tool_id);
            return true;
        }
        let shell_toggle = seg
            .shell_toggle_line
            .is_some_and(|line| source_line == Some(line));
        if shell_toggle {
            let tool_id = tool_id.to_owned();
            if !self.shell_raw.remove(&tool_id) {
                self.shell_raw.insert(tool_id.clone());
            }
            self.rebuild_expanded_tool(&tool_id);
            return true;
        }
        // A batch child answers for itself, before the card-wide expansion the
        // rest of the body falls back to.
        if let Some(target) = seg.row_target_at(rel, width) {
            let tool_id = tool_id.to_owned();
            self.toggle_batch_child(&tool_id, target.index());
            return true;
        }
        let tool_id = tool_id.to_owned();
        // A scroll card has two states, not three: the wheel is what reaches
        // the rest of the body, so a click has only the header to offer.
        if self.card_scrolls(&tool_id) {
            return self.close_card(&tool_id);
        }
        // The whole body is already on screen, so the only move left is the
        // way back: to the header where the mode left one, and to the resting
        // budget where it did not.
        if exp.full {
            return self.close_card(&tool_id) || self.rest_card(&tool_id);
        }
        // Resting and already complete, so there is nothing behind it to show.
        if !seg.truncation {
            return self.close_card(&tool_id);
        }
        self.open_card(&tool_id);
        true
    }

    /// Puts a card behind its header without a click, for when something else
    /// on screen already draws its body. Unlike [`Self::close_card`] this is
    /// not the reader's doing, so `card_can_close` does not gate it: a write is
    /// never collapsible by mode, and here that is beside the point.
    pub fn close_tool_card(&mut self, tool_id: &str) {
        self.disclosure
            .insert(tool_id.to_owned(), CardState::Closed);
        self.rebuild_tool_lines(tool_id);
    }

    /// Takes a card back to the single line it started as. The close is
    /// recorded rather than forgotten: auto would open this card again while
    /// it is the last one, and the reader just said not to.
    fn close_card(&mut self, tool_id: &str) -> bool {
        if !self.card_can_close(tool_id) || self.card_closed(tool_id) {
            return false;
        }
        self.disclosure
            .insert(tool_id.to_owned(), CardState::Closed);
        self.rebuild_expanded_tool(tool_id);
        true
    }

    /// Hands a card back to the view mode, which is the only way out of a full
    /// body in a transcript that gives every call a card and so has no header
    /// to close to.
    fn rest_card(&mut self, tool_id: &str) -> bool {
        if self.disclosure.remove(tool_id).is_none() {
            return false;
        }
        self.rebuild_expanded_tool(tool_id);
        true
    }

    #[cfg(test)]
    pub fn toggle_expansion_at(&mut self, row: u16, area: Rect) -> bool {
        self.handle_click(row, area)
    }

    /// One child opens or folds, which is what the reader asked for, and every
    /// other child stays exactly as it was.
    fn toggle_batch_child(&mut self, tool_id: &str, index: usize) {
        let views = self
            .batch_views
            .get(tool_id)
            .cloned()
            .unwrap_or_default()
            .toggled(index);
        self.batch_views.insert(tool_id.to_owned(), views);
        self.rebuild_tool_segment(tool_id);
    }

    fn rebuild_expanded_tool(&mut self, tool_id: &str) {
        if segment::is_instruction_segment(tool_id) {
            if let Some(parent_id) = segment::instruction_parent(tool_id)
                && let Some(parent_idx) = self.cache.find_by_tool_id(parent_id)
                && let Some(blocks) = self.get_instructions_for_tool(parent_id)
            {
                self.upsert_instruction_segment(parent_id, &blocks, parent_idx);
            }
        } else {
            self.rebuild_tool_segment(tool_id);
        }
    }

    fn get_instructions_for_tool(&self, tool_id: &str) -> Option<Vec<InstructionBlock>> {
        let msg = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))?;
        msg.tool_output.as_deref()?.owned_instructions()
    }

    /// Drains the highlight worker and every live tool buffer. These used to
    /// run inside [`Self::view`], which is why a running tool had to claim it
    /// was animating: it was the only way to keep them fed.
    pub fn tick(&mut self) -> Dirty {
        let mut dirty = self.drain_highlights() | self.poll_live_bufs();
        if self.show_idle_splash() {
            dirty |= self.idle_splash.poll_update(update::latest_notice());
        }
        dirty
    }

    pub fn cadence(&self) -> Cadence {
        // Collapsed thinking draws a line count, not the text, so its
        // typewriter reveals nothing and never advances either, since only
        // `view` ticks it. Believing it would pin the loop at full frame rate
        // for the whole reasoning phase.
        let smooth = self.streaming_text.is_animating()
            || self.accent.is_animating()
            || (self.streaming_thinking.is_animating() && !self.streaming_thinking_collapsed());
        Cadence::any([
            // A running tool draws a spinner. Its output arriving is data, and
            // `tick` reports that separately.
            Cadence::when(self.has_in_progress(), Cadence::SPINNER),
            Cadence::when(!self.streaming_thinking.is_empty(), Cadence::SPINNER),
            Cadence::when(smooth, Cadence::SMOOTH),
            Cadence::when(self.show_idle_splash(), self.idle_splash.cadence()),
        ])
    }

    fn streaming_reasoning_open(&self) -> bool {
        self.streaming_reasoning_open.unwrap_or(self.show_thinking)
    }

    /// What the reader asked of a settled block, or the gate when they have
    /// not asked. Reasoning is how the answer was reached rather than a call
    /// the transcript can summarise in a row, so no view mode closes it: the
    /// modes decide density among tool cards, and `show_thinking` is the only
    /// thing that decides whether reasoning starts open.
    ///
    /// An injected message is the other way round. It is reference material
    /// rather than part of the conversation, so it stays folded to its heading
    /// until someone asks for it.
    fn body_open(&self, msg: &DisplayMessage) -> bool {
        msg.body_open.unwrap_or(match msg.role {
            DisplayRole::Thinking => self.show_thinking,
            _ => false,
        })
    }

    fn has_foldable_body(msg: &DisplayMessage) -> bool {
        matches!(msg.role, DisplayRole::Thinking | DisplayRole::Injected)
    }

    fn streaming_thinking_collapsed(&self) -> bool {
        !self.streaming_reasoning_open() && !self.streaming_thinking.is_empty()
    }

    fn streaming_collapsed_height(&self, width: u16) -> u16 {
        let content_width =
            SegmentChrome::for_kind(SegmentKind::Thinking, width, 0).content_width(width);
        wrapped_line_count(&self.build_streaming_collapsed_lines(), content_width)
    }

    fn show_idle_splash(&self) -> bool {
        self.messages.is_empty()
            && self.streaming_thinking.is_empty()
            && self.streaming_text.is_empty()
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        has_selection: bool,
        message_actions_enabled: bool,
    ) {
        self.terminal_links.clear();
        self.message_action_hits.clear();
        if self.viewport_area != area {
            self.clear_hover();
            self.viewport_area = area;
        }
        let previous_scroll_top = self.scroll_top;
        let previous_total_lines = self.last_total_lines;
        let mut held = (!self.auto_scroll)
            .then(|| self.held_anchor.clone())
            .flatten();
        self.viewport_height = area.height;
        let width = area.width.saturating_sub(1);
        let theme_gen = theme::generation();
        let theme_changed = self.theme_generation != theme_gen;
        if self.viewport_width != width {
            self.clear_hover();
        }
        let width_changed = self.viewport_width != width || theme_changed;
        if width_changed {
            self.viewport_width = width;
            self.theme_generation = theme_gen;
            // A held height counts rows broken to a width, so the rows it
            // names stop existing the moment that width moves.
            self.card_floor.clear();
        }
        if theme_changed {
            self.rebake_stale_snapshots(theme_gen);
        }

        if self.show_idle_splash() {
            let accent = self.accent.resolve();
            self.idle_splash.render(area, frame.buffer_mut(), accent);
            return;
        }

        if width_changed {
            self.cache.mark_all_width_stale();
            let thinking = thinking_style();
            let streaming = match self.streaming_role {
                DisplayRole::User => user_style(),
                _ => assistant_style(),
            };
            self.streaming_thinking
                .set_style("", thinking.text_style, thinking.prefix_style);
            self.streaming_text.set_style(
                streaming.prefix,
                streaming.text_style,
                streaming.prefix_style,
            );
        }
        self.follow_latest();
        self.rebuild_line_cache();
        self.flush_dirty_cards();
        if let Some(seg_idx) = self.pending_scroll_segment.take() {
            self.scroll_to_segment(seg_idx.min(self.cache.len().saturating_sub(1)));
            // A density switch repositions the reader deliberately, so the
            // row they were on before it is no longer what they asked for.
            held = None;
        }
        if self.has_in_progress() {
            self.update_spinners();
            self.refresh_live_progress();
        }

        let cached_count = self.cache.len();
        let spacer_lines: [Line<'static>; 1] = [Line::default()];
        let mut streaming_heights: Vec<u16> = Vec::new();

        let thinking_collapsed = self.streaming_thinking_collapsed();
        let collapsed_thinking_lines = if thinking_collapsed {
            self.build_streaming_collapsed_lines()
        } else {
            Vec::new()
        };
        let mut expanded_thinking = None;

        // Streaming blocks live outside the cache and miss the margin pass,
        // so they take their blank row by hand. Measuring, painting,
        // selection, and hit-testing all walk these heights in order: one
        // extra entry in any of them shifts every row below it.
        if thinking_collapsed {
            if cached_count > 0 {
                streaming_heights.push(1);
            }
            let content_width =
                SegmentChrome::for_kind(SegmentKind::Thinking, width, 0).content_width(width);
            streaming_heights.push(wrapped_line_count(&collapsed_thinking_lines, content_width));
        } else if !self.streaming_thinking.is_empty() {
            let content_width =
                SegmentChrome::for_kind(SegmentKind::Thinking, width, 0).content_width(width);
            self.streaming_thinking.tick();
            let body = self.streaming_reasoning().body.to_owned();
            if self
                .streaming_thinking
                .update_render_from(body, content_width)
            {
                self.clear_hover();
            }
            let segment = self.streaming_thinking_segment();
            if cached_count > 0 {
                streaming_heights.push(1);
            }
            streaming_heights.push(wrapped_line_count(segment.lines(), content_width));
            expanded_thinking = Some(segment);
        }

        if !self.streaming_text.is_empty() {
            let content_width =
                SegmentChrome::for_kind(self.streaming_kind(), width, 0).content_width(width);
            if self.streaming_text.update_render(content_width) {
                self.clear_hover();
            }
            let lines = self.streaming_text.cached_lines();
            let has_previous = cached_count > 0 || !streaming_heights.is_empty();
            if has_previous {
                streaming_heights.push(1);
            }
            streaming_heights.push(wrapped_line_count(lines, content_width));
        }

        let streaming_sum: u32 = streaming_heights.iter().map(|&h| h as u32).sum();
        // The reflow window is picked from `scroll_top` and the bottom pin,
        // and the reflow changes the heights both are derived from: resolve
        // before to aim the window, and after to place the result.
        self.cache.update_margins(width);
        // Before the window is aimed rather than after it: the reflow reflows
        // what `scroll_top` points at, so putting the reader back first is
        // what makes it reflow the segments they are actually looking at.
        if let Some(anchor) = &held {
            self.restore_reading_anchor(anchor, width);
        }
        self.resolve_scroll(width, streaming_sum, has_selection);
        let pinned = self.reflow_viewport(width, has_selection);
        self.cache.update_margins(width);
        let total_lines = self.resolve_scroll(width, streaming_sum, has_selection);
        // A frame that could not re-pin slid the reader, and holding the row
        // it left them on would make that slide the thing every later frame
        // restores. Keeping the anchor they had is what lets the next frame,
        // whose indices have settled, put them back.
        if pinned {
            self.hold_reading_position();
        }
        if self.scroll_top != previous_scroll_top || total_lines != previous_total_lines {
            self.clear_hover();
        }

        let viewport = Rect::new(area.x, area.y, width, area.height);
        let mut cursor = RenderCursor::new(
            self.scroll_top,
            viewport,
            mem::take(&mut self.terminal_links),
        );

        let accent = self.accent.resolve();
        let mut message_action_hits = Vec::new();
        let mut windows: Vec<CardWindow> = Vec::new();
        for (i, seg) in self.cache.segments().iter().enumerate() {
            if cursor.past_bottom() {
                break;
            }
            let height = seg.height(width);
            // Everything below is per-segment work whose only consumer is the
            // paint, so a segment scrolled off the top is dropped before any
            // of it runs rather than inside `render`.
            if cursor.skip_above(height) {
                continue;
            }
            let highlight = self.highlight_segment == Some(i);
            let hover = self
                .hover_feedback_for_segment(seg)
                .map(|feedback| (feedback, accent));
            let source = message_actions_enabled
                .then(|| self.message_action_source(seg))
                .flatten();
            let message_action = source.map(|_| {
                (
                    matches!(self.hover, Some(HoverTarget::MessageAction(index)) if index == i),
                    accent,
                )
            });
            // Taken before the render, which consumes the cursor's skip, and
            // used after it, so the bars paint over the body they belong to.
            let placement = cursor.placement(height);
            let action_area = cursor.render(
                (seg.lines(), Some(seg.links())),
                height,
                seg.chrome(width),
                segment_styles(seg.kind(), accent, seg.compact),
                RenderFeedback {
                    highlight,
                    hover,
                    message_action,
                },
                frame,
            );
            if let Some(placement) = placement {
                grab_leaf!(seg.kind().grab_name(), placement.rect(viewport));
                if let Some(key) = window_key(seg) {
                    collect_card_windows(seg, &key, placement, width, viewport, &mut windows);
                }
            }
            if let (Some(area), Some(source)) = (action_area, source) {
                message_action_hits.push(MessageActionHit {
                    area,
                    target: MessageActionTarget {
                        source,
                        segment_index: i,
                    },
                });
            }
        }
        self.message_action_hits = message_action_hits;

        let mut height_idx = 0usize;
        let streamed: [(&StreamingContent, bool, SegmentKind); 2] = [
            (
                &self.streaming_thinking,
                thinking_collapsed,
                SegmentKind::Thinking,
            ),
            (&self.streaming_text, false, self.streaming_kind()),
        ];
        for (sc, collapsed, kind) in streamed {
            if sc.is_empty() || height_idx >= streaming_heights.len() || cursor.past_bottom() {
                continue;
            }
            if cached_count > 0 || height_idx > 0 {
                let h = streaming_heights[height_idx];
                height_idx += 1;
                #[cfg(debug_assertions)]
                let placement = cursor.placement(h);
                let _ = cursor.render(
                    (&spacer_lines, None),
                    h,
                    SegmentChrome::for_kind(SegmentKind::Assistant, width, 0),
                    (None, None),
                    RenderFeedback::default(),
                    frame,
                );
                grab_leaf!(
                    "transcript_spacer",
                    placement.map_or(Rect::ZERO, |at| at.rect(viewport))
                );
            }
            if height_idx < streaming_heights.len() {
                let h = streaming_heights[height_idx];
                height_idx += 1;
                let placement = cursor.placement(h);
                if collapsed {
                    let hover = matches!(self.hover, Some(HoverTarget::StreamingThinking))
                        .then_some((HoverFeedback::Chrome, accent));
                    let _ = cursor.render(
                        (&collapsed_thinking_lines, None),
                        h,
                        SegmentChrome::for_kind(kind, width, 0),
                        (None, None),
                        RenderFeedback {
                            hover,
                            ..RenderFeedback::default()
                        },
                        frame,
                    );
                } else if let (SegmentKind::Thinking, Some(segment)) = (kind, &expanded_thinking) {
                    let _ = cursor.render(
                        (segment.lines(), Some(segment.links())),
                        h,
                        segment.chrome(width),
                        (None, None),
                        RenderFeedback::default(),
                        frame,
                    );
                    if let Some(placement) = placement {
                        let key = ThinkingWindow::Live.key();
                        collect_card_windows(
                            segment,
                            &key,
                            placement,
                            width,
                            viewport,
                            &mut windows,
                        );
                    }
                } else {
                    let _ = cursor.render(
                        (sc.cached_lines(), Some(sc.links())),
                        h,
                        SegmentChrome::for_kind(kind, width, 0),
                        // Streaming text and reasoning, never a tool row,
                        // so compactness has no arm to reach here.
                        segment_styles(kind, accent, false),
                        RenderFeedback::default(),
                        frame,
                    );
                }
                grab_leaf!(
                    kind.grab_name(),
                    placement.map_or(Rect::ZERO, |at| at.rect(viewport))
                );
            }
        }
        // After the live blocks, so the live reasoning window gets its bar too.
        self.place_card_windows(windows, frame);
        self.terminal_links = cursor.into_terminal_links();

        if let Some(progress) = self.prompt_progress
            && let Some(bar) = prompt_progress::render(frame, area, progress, &self.prompt_rate)
        {
            self.terminal_links
                .retain(|link| !bar.contains(link.position));
        }

        if let Some(index) = self
            .cache
            .segment_at_row(self.scroll_top, self.viewport_width)
            .and_then(|(_, segment, _)| segment.msg_index)
        {
            self.scrollbar.set_hint(ScrollHint::messages(
                index as u32 + 1,
                self.cache.msg_count() as u32,
            ));
        }
        self.scrollbar
            .draw(frame, area, total_lines, self.scroll_top);
    }

    /// Records the windows that were drawn and paints a bar beside each,
    /// forgetting everything that is no longer on screen.
    ///
    /// A bar has to be placed every frame it exists, because placing is also
    /// what clears its track: a window that closed while a drag was live would
    /// otherwise keep the drag anchored to rows nothing draws any more. The
    /// arming follows for the same reason.
    fn place_card_windows(&mut self, windows: Vec<CardWindow>, frame: &mut Frame) {
        self.card_bars
            .retain(|key, _| windows.iter().any(|window| &window.key == key));
        if let Some(armed) = &self.armed_card
            && !windows.iter().any(|window| &window.key == armed)
        {
            self.armed_card = None;
        }
        for window in &windows {
            self.card_bars.entry(window.key.clone()).or_default().draw(
                frame,
                window.strip(),
                window.total,
                window.position,
            );
        }
        self.card_windows = windows;
    }

    fn window_at(&self, column: u16, row: u16) -> Option<&CardWindow> {
        let at = Position::new(column, row);
        self.card_windows
            .iter()
            .find(|window| window.body.contains(at))
    }

    #[cfg(test)]
    pub(crate) fn card_window_key_at(&self, column: u16, row: u16) -> Option<&str> {
        self.window_at(column, row)
            .map(|window| window.key.as_str())
    }

    #[cfg(test)]
    pub(crate) fn armed_card_key(&self) -> Option<&str> {
        self.armed_card.as_deref()
    }

    /// Arms the window under the pointer for the wheel, releasing whatever was
    /// armed before. `true` when a window took the press.
    ///
    /// Called on the press rather than the release because touch never reports
    /// a held drag, so the release path that resolves a click is skipped
    /// entirely under `ui.touch`. The press is also the gesture the reader
    /// performs, so it is the honest place for it.
    ///
    /// It deliberately does not consume the press: a sweep has to be able to
    /// start inside a card body, or the body's text could not be selected.
    pub fn arm_card_at(&mut self, column: u16, row: u16) -> bool {
        self.armed_card = self.window_at(column, row).map(|window| window.key.clone());
        self.armed_card.is_some()
    }

    /// Whether the armed window is the one under the pointer, which is what
    /// both the wheel and the click ask.
    ///
    /// The press says which window the reader means and the pointer says they
    /// still mean it. Position alone would hand a card every notch that passed
    /// over it on the way somewhere else.
    fn armed_at(&self, column: u16, row: u16) -> Option<&str> {
        self.window_at(column, row)
            .map(|window| window.key.as_str())
            .filter(|key| self.armed_card.as_deref() == Some(*key))
    }

    /// Whether a release at this point completes an arming press, in which
    /// case it must not also fold the card: a card's own control is its
    /// header, not the window the reader just aimed at.
    pub fn armed_card_at(&self, column: u16, row: u16) -> bool {
        self.armed_at(column, row).is_some()
    }

    /// Releases the armed window once the pointer is no longer inside it, so
    /// the wheel goes back to the transcript without needing a press to say
    /// so.
    fn disarm_unless_over(&mut self, column: u16, row: u16) {
        if self.armed_card.is_some()
            && self.window_at(column, row).map(|window| &window.key) != self.armed_card.as_ref()
        {
            self.armed_card = None;
        }
    }

    /// A card's bar takes the press before the transcript's own and before any
    /// selection: the strip is one column inside a card body, and both of the
    /// others would happily claim it.
    pub fn handle_card_scrollbar(&mut self, event: &MouseEvent) -> bool {
        let mut jump = None;
        let mut consumed = false;
        for (key, bar) in &mut self.card_bars {
            match bar.handle(event) {
                ScrollbarMouse::Ignored => continue,
                ScrollbarMouse::Consumed => consumed = true,
                ScrollbarMouse::ScrollTo(offset) => {
                    jump = Some((key.clone(), offset as usize));
                }
            }
            break;
        }
        match jump {
            Some((key, offset)) => {
                self.jump_window(&key, offset);
                true
            }
            None => consumed,
        }
    }

    /// The bar takes the press before any selection starts, or dragging it
    /// would sweep a selection down the transcript instead of scrolling it.
    pub fn handle_scrollbar(&mut self, event: &MouseEvent) -> bool {
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => false,
            ScrollbarMouse::Consumed => true,
            ScrollbarMouse::ScrollTo(top) => {
                self.set_scroll_top(top);
                true
            }
        }
    }

    /// The document row under a viewport row. Saturating because `scroll_top`
    /// starts at `u32::MAX` to pin the first frame to the bottom, and that
    /// sentinel stands until `resolve_scroll` has a height to clamp it
    /// against.
    fn doc_row(&self, row: u16, area: Rect) -> u32 {
        u32::from(row.saturating_sub(area.y)).saturating_add(self.scroll_top)
    }

    fn max_scroll(&self) -> u32 {
        self.last_total_lines
            .saturating_sub(u32::from(self.viewport_height))
    }

    pub fn scroll_top(&self) -> u32 {
        self.scroll_top
    }

    /// Backs `caudra.fn.winsaveview`. The clamp matters: a pinned or restored
    /// `scroll_top` can sit past the end until the next `view` resolves it
    /// against the current line count.
    pub fn win_view(&self) -> WinView {
        WinView {
            scroll_top: self.scroll_top.min(self.max_scroll()),
            line_count: self.last_total_lines,
            height: self.viewport_height,
            auto_scroll: self.auto_scroll,
        }
    }

    /// Raw markdown of the newest assistant reply. Streaming text has not
    /// been committed yet, so it is preferred when present.
    pub fn last_reply_source(&self) -> Option<String> {
        if !self.streaming_text.is_empty() {
            return Some(self.streaming_text.buffer().to_owned());
        }
        self.messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, DisplayRole::Assistant))
            .map(|m| m.text.clone())
    }

    /// Newest committed assistant reply, the default target for a review.
    /// Streaming text has no segment yet, so it cannot be reviewed.
    pub fn last_assistant_source(&self) -> Option<DisplaySource> {
        self.messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, DisplayRole::Assistant))
            .and_then(|m| m.source)
    }

    /// Painted lines and provenance for the segment behind `source`, so the
    /// review modal can re-lay them at its own width and quote the markdown
    /// that produced them.
    pub(crate) fn review_target(&self, source: DisplaySource) -> Option<ReviewTarget> {
        let segment = self
            .cache
            .segments()
            .iter()
            .find(|segment| self.segment_source(segment) == Some(source))?;
        if segment.lines().is_empty() {
            return None;
        }
        Some(ReviewTarget {
            lines: segment.lines().to_vec(),
            provenance: segment.provenance().cloned(),
            label: review_label(source),
        })
    }

    pub fn segment_heights(&self) -> Vec<u16> {
        let width = self.viewport_width;
        self.cache
            .segments()
            .iter()
            .map(|s| s.height(width))
            .collect()
    }

    pub fn segment_search_texts(&self) -> Vec<&str> {
        self.cache.search_texts()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        let mut fragments =
            selection::extract_selection_fragments(&self.cache, self.viewport_width, sel, msg_area);
        self.append_streaming_selection_fragments(&mut fragments, sel, msg_area);
        if fragments.len() <= 1 {
            return selection::join_fragments(&fragments);
        }
        self.format_selection_markdown(&fragments)
    }

    fn append_streaming_selection_fragments(
        &self,
        fragments: &mut Vec<selection::SelectionFragment>,
        sel: &Selection,
        msg_area: Rect,
    ) {
        let width = self.viewport_width;
        let cached_count = self.cache.len();
        let mut segment_start = self.cache.total_height(width);

        if !self.streaming_thinking.is_empty() {
            let mut segment = if self.streaming_thinking_collapsed() {
                let mut segment = Segment::with_lines(
                    self.build_streaming_collapsed_lines(),
                    String::new(),
                    None,
                );
                segment.set_kind(SegmentKind::Thinking);
                segment
            } else {
                self.streaming_thinking_segment()
            };
            segment.set_margin_top(u16::from(cached_count > 0));
            if let Some(fragment) =
                selection::extract_segment_fragment(&segment, segment_start, width, sel, msg_area)
            {
                fragments.push(fragment);
            }
            segment_start += u32::from(segment.height(width));
        }

        if self.streaming_text.is_empty() {
            return;
        }
        let mut segment = Segment::with_lines(
            self.streaming_text.cached_lines().to_vec(),
            String::new(),
            None,
        );
        segment.set_kind(self.streaming_kind());
        segment.set_margin_top(u16::from(
            cached_count > 0 || !self.streaming_thinking.is_empty(),
        ));
        segment.set_provenance(self.streaming_text.provenance().cloned());
        if let Some(fragment) =
            selection::extract_segment_fragment(&segment, segment_start, width, sel, msg_area)
        {
            fragments.push(fragment);
        }
    }

    fn format_selection_markdown(&self, fragments: &[selection::SelectionFragment]) -> String {
        let mut document = String::new();
        for fragment in fragments {
            let message = fragment
                .msg_index
                .and_then(|index| self.messages.get(index));
            let (heading, metadata, body, fenced) = match fragment.kind {
                SegmentKind::User => ("User".to_owned(), None, fragment.text.as_str(), false),
                SegmentKind::TaskDelivery => (
                    "Task delivery".to_owned(),
                    None,
                    fragment.text.as_str(),
                    false,
                ),
                SegmentKind::AutomationDelivery => (
                    "Automation delivery".to_owned(),
                    None,
                    fragment.text.as_str(),
                    false,
                ),
                SegmentKind::PeerMessage => (
                    "Peer message".to_owned(),
                    None,
                    fragment.text.as_str(),
                    true,
                ),
                SegmentKind::Assistant => {
                    match message.and_then(|message| message.plan_path.as_deref()) {
                        Some(plan_path) => (
                            "Assistant Plan".to_owned(),
                            Some(format!("Path: {}", markdown_code_span(plan_path))),
                            fragment.text.as_str(),
                            false,
                        ),
                        None => ("Assistant".to_owned(), None, fragment.text.as_str(), false),
                    }
                }
                SegmentKind::Thinking => {
                    let open = message.map_or_else(
                        || self.streaming_reasoning_open(),
                        |message| self.body_open(message),
                    );
                    let summary = match message {
                        Some(message) => reasoning_summary(&message.text),
                        None if open => self.streaming_reasoning(),
                        None => reasoning_summary(self.streaming_thinking.buffer()),
                    };
                    let heading = summary.title.map_or_else(
                        || "Thinking".to_owned(),
                        |title| format!("Thinking: {}", markdown_inline(title)),
                    );
                    let duration = match message {
                        Some(message) => message.thinking_duration.map(format_settled_duration),
                        None => self
                            .thinking_started
                            .map(|started| format_live_duration(live_elapsed(started))),
                    };
                    let mut metadata = format!("View: {}", if open { "open" } else { "collapsed" });
                    if let Some(duration) = duration {
                        metadata.push_str(" | Duration: ");
                        metadata.push_str(&duration);
                    }
                    (
                        heading,
                        Some(metadata),
                        if open && !summary.body.is_empty() {
                            fragment.text.as_str()
                        } else {
                            ""
                        },
                        false,
                    )
                }
                SegmentKind::ToolInline | SegmentKind::ToolBlock | SegmentKind::Instruction => {
                    let segment_tool_id = fragment.tool_id.as_deref().unwrap_or("unknown");
                    let instruction = segment::is_instruction_segment(segment_tool_id);
                    let parent_id =
                        segment::instruction_parent(segment_tool_id).unwrap_or(segment_tool_id);
                    let tool_message = self.tool_card(parent_id).and_then(|(index, tool)| {
                        self.messages.get(index).map(|message| (message, tool))
                    });
                    let tool_name = tool_message.map_or(parent_id, |(_, tool)| tool.name.as_ref());
                    let heading = if instruction {
                        format!("Instructions: {}", markdown_code_span(tool_name))
                    } else {
                        format!("Tool: {}", markdown_code_span(tool_name))
                    };
                    let open = self
                        .tool_expansion(segment_tool_id, self.card_opens_by_default(parent_id))
                        .is_some();
                    let mut metadata = if instruction {
                        format!("View: {}", if open { "open" } else { "collapsed" })
                    } else {
                        let status = tool_message
                            .map_or("unknown", |(_, tool)| tool_status_label(tool.status));
                        format!(
                            "Status: {status} | View: {}",
                            if open { "open" } else { "collapsed" }
                        )
                    };
                    if let Some(annotation) =
                        tool_message.and_then(|(message, _)| message.annotation.as_deref())
                    {
                        metadata.push_str(" | Annotation: ");
                        metadata.push_str(&markdown_inline(annotation));
                    }
                    // A batch copies as its children's sections, each already
                    // fenced where it needed to be, so fencing the card too
                    // would flatten the hierarchy back into one block. A
                    // selection that stayed inside one child's code still names
                    // a language, and that still gets a fence of its own.
                    let fenced =
                        fragment.language.is_some() || !names_tool(BATCH_TOOL_NAME, tool_name);
                    self.append_selection_section(
                        &mut document,
                        &heading,
                        Some(&metadata),
                        fragment.text.as_str(),
                        fenced,
                        fragment.language.as_deref(),
                    );
                    continue;
                }
                SegmentKind::Error => ("Error".to_owned(), None, fragment.text.as_str(), true),
                SegmentKind::Done => ("Done".to_owned(), None, fragment.text.as_str(), true),
            };
            self.append_selection_section(
                &mut document,
                &heading,
                metadata.as_deref(),
                body,
                fenced,
                None,
            );
        }
        document
    }

    /// `language` names the fence when the body is one language's source, and
    /// is ignored when the body is not fenced at all.
    fn append_selection_section(
        &self,
        document: &mut String,
        heading: &str,
        metadata: Option<&str>,
        body: &str,
        fenced: bool,
        language: Option<&str>,
    ) {
        if !document.is_empty() {
            document.push_str("\n\n---\n\n");
        }
        document.push_str("## ");
        document.push_str(heading);
        if let Some(metadata) = metadata {
            document.push_str("\n\n_");
            document.push_str(metadata);
            document.push('_');
        }
        if body.is_empty() {
            return;
        }
        document.push_str("\n\n");
        if fenced {
            document.push_str(&fenced_text(body, language));
        } else {
            let caudra_block = unclosed_caudra_fenced_block(body);
            let commonmark_block = unclosed_markdown_block(body);
            if caudra_block != commonmark_block {
                // Conflicting parser states have no shared invisible closer.
                document.push_str(&fenced_text(
                    &rendered_markdown_text(body, self.viewport_width),
                    language,
                ));
            } else {
                document.push_str(body);
            }
            if let Some(block) = caudra_block.filter(|_| caudra_block == commonmark_block) {
                if !document.ends_with('\n') {
                    document.push('\n');
                }
                match block {
                    MarkdownBlock::Fence(marker, run) => {
                        document.extend(std::iter::repeat_n(marker, run));
                    }
                    MarkdownBlock::Html(closing) => document.push_str(closing),
                    MarkdownBlock::Math(closing) => document.push_str(closing),
                    MarkdownBlock::HtmlBlankLine => {}
                }
            }
        }
    }

    fn tool_in_progress(&self, tool_id: &str) -> bool {
        self.messages
            .iter()
            .rev()
            .find_map(|m| match &m.role {
                DisplayRole::Tool(t) if t.id == tool_id => Some(t.status),
                _ => None,
            })
            .is_some_and(|s| s == ToolStatus::InProgress)
    }

    fn active_shell_call(&self, call_id: &str) -> bool {
        self.shell_call(call_id).is_some_and(TaskCard::active)
    }

    fn shell_call(&self, call_id: &str) -> Option<&TaskCard> {
        self.task_cards
            .get(call_id)
            .or_else(|| {
                self.messages
                    .iter()
                    .rev()
                    .filter_map(|message| message.tool_output.as_deref())
                    .find_map(|output| task_card::find_call(output, call_id))
            })
            .filter(|task| task.kind == JobKind::Shell)
    }

    fn root_tool_id<'a>(&self, mut call_id: &'a str) -> Option<&'a str> {
        loop {
            if self.tool_card(call_id).is_some() {
                return Some(call_id);
            }
            call_id = batch_child_id(call_id)?.0;
        }
    }

    fn watching(&self, tool_id: &str) -> bool {
        self.watched_bufs.iter().any(|(id, _)| id == tool_id)
    }

    fn stop_watching(&mut self, tool_id: &str) {
        self.watched_bufs.retain(|(id, _)| id != tool_id);
    }

    /// Moves a finished tool's live buf to the watched set, flushing any
    /// last dirty lines. Called on completion and on cancellation, so
    /// `live_bufs` never leaks entries that outlive their tool, and the capped
    /// watched set keeps what `tick` polls bounded.
    /// Returns whether a live buf existed for this id.
    fn retire_live_buf(&mut self, id: &str) -> bool {
        let Some(buf) = self.live_bufs.remove(id) else {
            return false;
        };
        if let Some(lines) = buf.read_if_dirty() {
            self.store_snapshot(id, BufferSnapshot::from_arc(lines), false, None);
        }
        self.watched_bufs.push_back((id.to_owned(), buf));
        if self.watched_bufs.len() > WARM_TOOL_CAP {
            self.watched_bufs.pop_front();
        }
        true
    }

    fn has_snapshot(&self, tool_id: &str) -> bool {
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
            .is_some_and(|m| m.render_snapshot.is_some())
    }

    fn lua_restore_item(&self, tool_id: &str) -> Option<caudra_lua::RestoreItem> {
        let msg = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))?;
        crate::chat::restore_item_for(msg, self.tool_output_lines, self.theme_generation)
    }

    /// Re-restores every snapshot still painted with old-theme colors.
    /// Replies carry a generation so stale ones can't overwrite fresher colors.
    fn rebake_stale_snapshots(&mut self, current_gen: u64) {
        let Some(tx) = self.restore_event_tx.clone() else {
            return;
        };
        if self.lua_event_handle.is_disconnected() {
            return;
        }
        let eh = &self.lua_event_handle;
        self.rebake_requested.retain(|_, g| *g >= current_gen);
        let tol = self.tool_output_lines;
        let mut requested = Vec::new();
        for msg in &self.messages {
            let DisplayRole::Tool(role) = &msg.role else {
                continue;
            };
            if !self.should_request_rebake(
                &role.id,
                msg.snapshot_is_stale(current_gen),
                current_gen,
            ) {
                continue;
            }
            if let Some(mut item) = crate::chat::restore_item_for(msg, tol, current_gen) {
                item.clicks = self.lua_clicks.get(&role.id).cloned().unwrap_or_default();
                eh.request_restore(item, tx.clone());
                requested.push(role.id.clone());
            }
        }
        for id in requested {
            // The watched buf still carries old-theme lines; clicks in
            // the rebake window must go through restore, not warm.
            self.stop_watching(&id);
            self.rebake_requested.insert(id, current_gen);
        }
    }

    fn should_request_rebake(&self, tool_id: &str, stale: bool, current_gen: u64) -> bool {
        stale && self.rebake_requested.get(tool_id) != Some(&current_gen)
    }

    /// Live snapshots (`None`) get the panel's current generation.
    /// Re-bake replies are monotonic: drop if something newer landed.
    fn resolve_snapshot_gen(&self, tool_id: &str, incoming: Option<u64>) -> Option<u64> {
        let Some(incoming_gen) = incoming else {
            return Some(self.theme_generation);
        };
        match self.current_snapshot_gen(tool_id) {
            Some(applied) if applied > incoming_gen => None,
            _ => Some(incoming_gen),
        }
    }

    fn current_snapshot_gen(&self, tool_id: &str) -> Option<u64> {
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
            .map(|m| m.snapshot_theme_gen)
    }

    fn store_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        is_header: bool,
        theme_gen: Option<u64>,
    ) {
        if theme_gen.is_none() && self.shell_call(tool_id).is_some_and(|task| !task.active()) {
            return;
        }
        if self.tool_card(tool_id).is_none()
            && let Some((parent, index)) = batch_child_id(tool_id)
        {
            if !is_header {
                self.set_batch_child_output(parent, index, &snapshot.text());
            }
            return;
        }
        if theme_gen.is_none()
            && let Some((index, tool)) = self.tool_card(tool_id)
        {
            let msg = &self.messages[index];
            if tool.status != ToolStatus::InProgress
                && matches!(msg.tool_output.as_deref(), Some(ToolOutput::Shell(_)))
            {
                return;
            }
            if !is_header && tool.name.as_ref() == SHELL_TOOL_NAME {
                self.tool_output(tool_id, &snapshot.text());
            }
        }
        if theme_gen.is_some() {
            // A generation only comes with restore replies. The restore
            // superseded the old live view (and evicted the runtime's
            // warm handle), so its buf must not overwrite this snapshot.
            self.stop_watching(tool_id);
        }
        let Some(applied_gen) = self.resolve_snapshot_gen(tool_id, theme_gen) else {
            return;
        };
        if let Some(msg) = self.find_tool_msg_mut(tool_id) {
            if is_header {
                msg.text = snapshot.first_line_text();
                msg.render_header = Some(snapshot);
            } else {
                msg.render_snapshot = Some(snapshot);
            }
            msg.snapshot_theme_gen = applied_gen;
            // A live shell chunk reaches `tool_output` above on its way here,
            // so the card it owes a redraw is the one this draws right now.
            self.dirty_cards.remove(tool_id);
            self.rebuild_tool_segment(tool_id);
        } else {
            self.dropped_snapshots.record(tool_id);
        }
    }

    fn find_tool_msg_mut(&mut self, tool_id: &str) -> Option<&mut DisplayMessage> {
        self.messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
    }

    /// The context a single card is built in. Compactness is per call rather
    /// than per panel: an always-collapsed tool draws its row inside an
    /// expanded transcript, and the row's width comes from the chrome that
    /// row actually gets.
    fn rctx(&self, tool: &str, tool_id: &str) -> RenderCtx<'_> {
        let compact = self.draws_compact(tool);
        let kind = if compact {
            SegmentKind::ToolInline
        } else {
            SegmentKind::ToolBlock
        };
        let at = self.card_scroll.get(tool_id).copied().unwrap_or_default();
        RenderCtx {
            started_at: self.started_at,
            width: SegmentChrome::for_kind(kind, self.viewport_width, 0)
                .content_width(self.viewport_width),
            tool_output_lines: &self.tool_output_lines,
            card_scroll: self.policy.window(tool, at.offset, at.follow),
            child_scroll: self.child_windows(tool_id),
            policy: self.policy.clone(),
            compact,
            batch_views: &self.batch_views,
            batch_progress: &self.batch_child_progress,
            batch_live: &self.batch_child_output,
            batch_started: &self.batch_child_started,
            task_cards: Some(&self.task_cards),
            cwd: self.cwd.clone(),
        }
    }

    /// Where every scrolling child of this card has its window. Built from
    /// the roster rather than from the scroll map, so a child the reader has
    /// never touched still gets the window its tool asks for.
    fn child_windows(&self, tool_id: &str) -> Arc<HashMap<usize, ScrollWindow>> {
        let Some(ToolOutput::Batch { entries, .. }) = self
            .tool_card(tool_id)
            .and_then(|(idx, _)| self.messages[idx].tool_output.as_deref())
        else {
            return Arc::default();
        };
        let windows: HashMap<usize, ScrollWindow> = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let at = self
                    .card_scroll
                    .get(&child_scroll_id(tool_id, index))
                    .copied()
                    .unwrap_or_default();
                Some((
                    index,
                    self.policy.window(&entry.tool, at.offset, at.follow)?,
                ))
            })
            .collect();
        Arc::new(windows)
    }

    pub fn register_live_buf(&mut self, id: String, body: Arc<SharedBuf>) {
        if self.active_shell_call(&id) {
            self.live_bufs.insert(id, body);
            return;
        }
        if let Some((_, tool)) = self.tool_card(&id) {
            if tool.status != ToolStatus::InProgress {
                return;
            }
        } else if let Some((parent, index)) = batch_child_id(&id)
            && !self.batch_child_running(parent, index)
        {
            return;
        }
        self.live_bufs.insert(id, body);
    }

    fn remove_child_live_bufs(&mut self, parent: &str) {
        let prefix = format!("{parent}:");
        self.live_bufs.retain(|id, _| !id.starts_with(&prefix));
    }

    /// Snapshots are baked at the last width `view` saw, and a resize
    /// invalidates every segment anyway (see `width_changed` in `view`), so
    /// polling ahead of the frame that reflows them is safe.
    fn poll_live_bufs(&mut self) -> Dirty {
        let updated: Vec<_> = self
            .live_bufs
            .iter()
            .chain(self.watched_bufs.iter().map(|(id, buf)| (id, buf)))
            .filter_map(|(id, buf)| buf.read_if_dirty().map(|lines| (id.clone(), lines)))
            .collect();
        let dirty = Dirty::from(!updated.is_empty());
        for (tool_id, lines) in updated {
            self.store_snapshot(&tool_id, BufferSnapshot::from_arc(lines), false, None);
        }
        dirty
    }

    fn build_tool_segment_lines(
        msg: &DisplayMessage,
        status: ToolStatus,
        rctx: &RenderCtx,
        exp: Option<Disclosure>,
    ) -> ToolLines {
        let mut tl = build_tool_lines(msg, status, rctx, exp);
        // A compact row is meant to be scannable, and a right-aligned clock
        // on every line is the opposite of that.
        if let Some(ts) = &msg.timestamp
            && !rctx.compact
            && !tl.lines.is_empty()
        {
            append_right_info(
                &mut tl.lines[0],
                msg.turn_usage.as_deref(),
                Some(ts),
                rctx.width,
            );
            tl.links.rows[0] = vec![None; tl.lines[0].spans.len()];
        }
        tl
    }

    /// Builds a card, holding a still-running one to the tallest it has
    /// already drawn.
    ///
    /// The shortfall comes out of the scrollback its windows are already
    /// holding and nowhere else: a window that has shown `n` rows reopens on
    /// `n` rows of buffered content, so a height that is held is a height full
    /// of something to read. A card with nothing left to reveal is returned as
    /// built, because the alternative is blank rows that take clicks and hover
    /// for a card that gave them up.
    fn build_held_tool_lines(
        msg: &DisplayMessage,
        status: ToolStatus,
        rctx: &RenderCtx,
        exp: Option<Disclosure>,
        floor: usize,
    ) -> ToolLines {
        let tl = Self::build_tool_segment_lines(msg, status, rctx, exp);
        if status != ToolStatus::InProgress {
            return tl;
        }
        let plan = refill_plan(&tl, floor.saturating_sub(tl.lines.len()));
        if plan.is_empty() {
            return tl;
        }
        Self::build_tool_segment_lines(msg, status, &rctx.with_windows_opened(&plan), exp)
    }

    fn build_anchored_tool_lines(
        &mut self,
        tool_id: &str,
        msg_index: usize,
        status: ToolStatus,
        exp: Option<Disclosure>,
    ) -> ToolLines {
        let floor = self.card_floor.get(tool_id).copied().unwrap_or_default();
        let build = |panel: &Self| {
            let msg = &panel.messages[msg_index];
            let rctx = panel.rctx(msg.role.tool_name().unwrap_or_default(), tool_id);
            Self::build_held_tool_lines(msg, status, &rctx, exp, floor)
        };
        let lines = build(self);
        if self.rebase_history_windows(tool_id, &lines.scroll_spans) {
            build(self)
        } else {
            lines
        }
    }

    fn rebase_history_windows(&mut self, tool_id: &str, spans: &[ScrollSpan]) -> bool {
        let mut changed = false;
        for span in spans {
            let child_key = span.child.map(|index| child_scroll_id(tool_id, index));
            let key = child_key.as_deref().unwrap_or(tool_id);
            if let Some(scroll) = self.card_scroll.get_mut(key) {
                changed |= scroll.rebase_history(span.history_start);
            }
        }
        changed
    }

    /// Records what a running call's card drew, and gives the floor back the
    /// moment the call settles so the one contraction a card makes lands
    /// where the reader is already expecting a change.
    fn hold_card_height(&mut self, tool_id: &str, status: ToolStatus, rows: usize) {
        if status != ToolStatus::InProgress {
            self.card_floor.remove(tool_id);
            return;
        }
        // Looked up before it is inserted: this runs for every running card on
        // every frame, and the entry is already there for all but the first.
        if let Some(held) = self.card_floor.get_mut(tool_id) {
            *held = (*held).max(rows);
        } else {
            self.card_floor.insert(tool_id.to_owned(), rows);
        }
    }

    fn flush_thinking(&mut self) {
        let started = self.thinking_started.take();
        if self.streaming_thinking.is_empty() {
            return;
        }
        let mut msg =
            DisplayMessage::new(DisplayRole::Thinking, self.streaming_thinking.take_all());
        msg.body_open = self.streaming_reasoning_open.take();
        msg.thinking_duration = started.map(live_elapsed);
        // Where the reader left the live window is theirs, not the stream's, so
        // the settled block opens on the same rows with the wheel still armed.
        self.rekey_window(
            &ThinkingWindow::Live.key(),
            ThinkingWindow::Settled(self.messages.len()).key(),
        );
        self.messages.push(msg);
    }

    /// The two halves the reveal has earned so far. The buffer decides which
    /// of them a fragment belongs to, so an unfinished heading is withheld
    /// instead of being drawn as body and then moved into the header.
    fn streaming_reasoning(&self) -> ReasoningSummary<'_> {
        streaming_reasoning_summary(
            self.streaming_thinking.visible(),
            self.streaming_thinking.buffer(),
        )
    }

    fn build_streaming_collapsed_lines(&self) -> Vec<Line<'static>> {
        thought_line(
            reasoning_summary(self.streaming_thinking.buffer()).title,
            self.thinking_started.map(live_elapsed),
            false,
        )
    }

    /// The open live block as the segment it will settle into: the rows, links,
    /// source and window that the paint, the pointer and copy all read, so none
    /// of them can disagree about which rows the window took.
    fn streaming_thinking_segment(&self) -> Segment {
        let summary = self.streaming_reasoning();
        let header = thought_line(
            summary.title,
            self.thinking_started.map(live_elapsed),
            false,
        );
        let content = &self.streaming_thinking;
        // A body with nothing in it still renders one line, which would leave
        // the card a row taller than the header it draws.
        let built = if summary.body.is_empty() || content.cached_lines().is_empty() {
            BuiltMessage::bare(header, String::new())
        } else {
            thinking::assemble(
                header,
                thinking::Body {
                    lines: content.cached_lines(),
                    links: content.links(),
                    provenance: content.provenance(),
                    diagrams: &[],
                },
                self.thinking_window(ThinkingWindow::Live),
                ScrollTail::Resumable,
            )
        };
        let mut segment = Segment::default();
        segment.set_kind(SegmentKind::Thinking);
        install_built(&mut segment, built);
        segment
    }

    fn build_cached_thinking_indicator(
        &self,
        text: &str,
        duration: Option<Duration>,
    ) -> Vec<Line<'static>> {
        thought_line(reasoning_summary(text).title, duration, true)
    }

    /// The one row a folded message draws, plus the text search still has to
    /// match against. Both fold paths read it so a rebuild cannot disagree with
    /// a reflow about what a closed block looks like.
    fn folded_message(&self, msg: &DisplayMessage) -> BuiltMessage {
        match msg.role {
            DisplayRole::Injected => BuiltMessage::bare(
                injected_line(&msg.text),
                format!("{INJECTED_SEARCH_PREFIX}{}", msg.text),
            ),
            _ => BuiltMessage::bare(
                self.build_cached_thinking_indicator(&msg.text, msg.thinking_duration),
                format!("{THINKING_SEARCH_PREFIX}{}", msg.text),
            ),
        }
    }

    /// A message as its segment draws it at `width`: folded to its one row, or
    /// whole with its diagram pans and, for reasoning, the window the reader
    /// left it at.
    fn built_message(&self, msg_index: usize, width: u16) -> Option<BuiltMessage> {
        let msg = self.messages.get(msg_index)?;
        if Self::has_foldable_body(msg) && !self.body_open(msg) {
            return Some(self.folded_message(msg));
        }
        let window = matches!(msg.role, DisplayRole::Thinking)
            .then(|| self.thinking_window(ThinkingWindow::Settled(msg_index)))
            .flatten();
        Some(build_message_lines(
            msg,
            width,
            self.pans_for(msg_index),
            window,
        ))
    }

    fn try_toggle_collapsed_thinking(&mut self, doc_row: u32, width: u16) -> bool {
        if !self.is_collapsed_streaming_thinking_row(doc_row, width) {
            return false;
        }
        self.streaming_reasoning_open = Some(true);
        true
    }

    /// The live window's footer re-pins it to the newest rows, as a settled
    /// block's does.
    fn try_follow_streaming_thinking(&mut self, doc_row: u32, width: u16) -> bool {
        if self.streaming_thinking.is_empty() || self.streaming_thinking_collapsed() {
            return false;
        }
        let spacer = u32::from(self.cache.len() > 0);
        let thinking_start = self.cache.total_height(width) + spacer;
        let Some(rel) = doc_row
            .checked_sub(thinking_start)
            .and_then(|rel| u16::try_from(rel).ok())
        else {
            return false;
        };
        if !on_scroll_footer(&self.streaming_thinking_segment(), rel, width) {
            return false;
        }
        self.follow_thinking(ThinkingWindow::Live);
        true
    }

    fn is_collapsed_streaming_thinking_row(&self, doc_row: u32, width: u16) -> bool {
        if !self.streaming_thinking_collapsed() {
            return false;
        }
        let spacer = u32::from(self.cache.len() > 0);
        let thinking_start = self.cache.total_height(width) + spacer;
        let height = self.streaming_collapsed_height(width) as u32;
        doc_row >= thinking_start && doc_row < thinking_start + height
    }

    /// Reasoning opens by default, so the click is the only way to fold a
    /// long block back to its header. A drag that selects text never reaches
    /// here, so the body stays clickable without swallowing selections.
    fn try_toggle_cached_thinking(&mut self, msg_idx: Option<usize>, width: u16) -> bool {
        let Some(idx) = msg_idx else { return false };
        let Some(msg) = self.messages.get(idx) else {
            return false;
        };
        if !Self::has_foldable_body(msg) {
            return false;
        }
        let open = self.body_open(msg);
        self.messages[idx].body_open = Some(!open);
        self.rebuild_thinking_segment(idx, width);
        true
    }

    fn rebuild_thinking_segment(&mut self, msg_idx: usize, width: u16) {
        let Some(built) = self.built_message(msg_idx, width) else {
            return;
        };
        if let Some(seg) = self
            .cache
            .segments_mut()
            .iter_mut()
            .find(|s| s.msg_index == Some(msg_idx) && s.tool_id.is_none())
        {
            install_built(seg, built);
        }
    }

    fn update_spinners(&mut self) {
        let spinner_span = Span::styled(
            spinner_str(self.started_at.elapsed().as_millis()),
            theme::current().spinner,
        );
        for seg in self.cache.segments_mut() {
            seg.update_spinners(&spinner_span);
        }
    }

    /// The cards whose text is a function of the wall clock: a running
    /// subagent's activity row, and a running shell call's header clock. A
    /// chatty command already rebuilds far more often than this through
    /// `tool_output`, and the rebuild reuses its highlight results.
    fn refresh_live_progress(&mut self) {
        let progress = &self.batch_child_progress;
        let started = &self.batch_child_started;
        let live: Vec<String> = self
            .messages
            .iter()
            .filter_map(|msg| {
                let DisplayRole::Tool(tool) = &msg.role else {
                    return None;
                };
                // A batch keeps a clock per child, since the reports belong to
                // rows the card has no header for and it carries none itself.
                // The header clock asks the renderer's own question, so the set
                // refreshed here cannot drift from the set whose clock moves.
                // Every settled shell draws a clock too, and counting those
                // rebuilt each one in the transcript on every frame.
                let ticking = shell_clock_ticks(msg, tool.status)
                    || msg.progress.as_ref().is_some_and(ToolProgress::is_live)
                    || progress
                        .get(&tool.id)
                        .is_some_and(|children| children.values().any(ToolProgress::is_live))
                    // Emptied as each child lands, so an entry that survives is
                    // a roster row still counting up.
                    || started.get(&tool.id).is_some_and(|children| !children.is_empty());
                ticking.then(|| tool.id.clone())
            })
            .collect();
        for tool_id in live {
            self.rebuild_tool_lines(&tool_id);
        }
    }

    fn drain_highlights(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Some(result) = self.hl_worker.try_recv() {
            if let Some(seg) = self
                .cache
                .segments_mut()
                .iter_mut()
                .find(|s| s.matches_pending_highlight(result.id))
            {
                seg.apply_highlight_result(
                    result.id,
                    result.lines,
                    result.rows,
                    result.source_rows,
                    &self.hl_worker,
                );
                dirty = Dirty::YES;
            }
        }
        dirty
    }

    fn rebuild_tool_segment(&mut self, tool_id: &str) {
        self.clear_hover();
        self.rebuild_tool_lines(tool_id);
    }

    /// Leaves hover alone, so the clock-driven refresh cannot cancel the
    /// reader's pointer every frame.
    fn rebuild_tool_lines(&mut self, tool_id: &str) {
        let Some((msg_idx, t)) = self.tool_card(tool_id) else {
            return;
        };
        let (status, opens) = (t.status, self.opens_by_default(t, msg_idx));
        let Some(seg_idx) = self.cache.find_by_tool_id(tool_id) else {
            return;
        };

        let exp = self.tool_expansion(tool_id, opens);
        // Before the build: the syntax set may finish loading while it runs.
        let ready = highlight::is_ready();
        let tl = self.build_anchored_tool_lines(tool_id, msg_idx, status, exp);
        let msg = &self.messages[msg_idx];

        let instructions = msg
            .tool_output
            .as_deref()
            .and_then(|o| o.owned_instructions());

        let compact = self.draws_compact(msg.role.tool_name().unwrap_or_default());
        let drawn = tl.lines.len();
        let seg = self.cache.get_mut(seg_idx).unwrap();
        seg.search_text = tl.search_text.clone();
        seg.update_with_reuse(tl, &self.hl_worker, compact);
        seg.uncoloured = !ready;
        self.hold_card_height(tool_id, status, drawn);

        if let Some(blocks) = instructions {
            self.upsert_instruction_segment(tool_id, &blocks, seg_idx);
        }
    }

    fn rebuild_line_cache(&mut self) {
        if !self.cache.needs_rebuild(self.messages.len()) {
            return;
        }
        // A full rebuild draws every card at once, so it colours only the
        // commands already coloured, and no card is coloured before the
        // syntax set loads. `reflowed_height` colours the rest as they near
        // the screen.
        let full = self.cache.msg_count() == 0;
        let ready = highlight::is_ready();
        for i in self.cache.msg_count()..self.messages.len() {
            let msg = &self.messages[i];

            if let DisplayRole::Tool(t) = &msg.role {
                let exp = self.tool_expansion(&t.id, self.opens_by_default(t, i));
                let status = t.status;
                let id = t.id.clone();
                let compact = self.draws_compact(&t.name);
                let blocks = msg
                    .tool_output
                    .as_deref()
                    .and_then(|o| o.owned_instructions());
                let (tl, uncoloured) = if full {
                    command_text::deferring(|| self.build_anchored_tool_lines(&id, i, status, exp))
                } else {
                    (self.build_anchored_tool_lines(&id, i, status, exp), !ready)
                };
                let drawn = tl.lines.len();
                let mut seg = Segment::with_tool(id.clone(), SegmentKind::ToolBlock, Some(i));
                seg.search_text = tl.search_text.clone();
                seg.apply_highlight(tl, &self.hl_worker, compact);
                seg.uncoloured = uncoloured;
                self.cache.push(seg);
                self.hold_card_height(&id, status, drawn);

                if let Some(blocks) = blocks {
                    let last_idx = self.cache.len().saturating_sub(1);
                    self.upsert_instruction_segment(&id, &blocks, last_idx);
                }
            } else {
                let kind = segment_kind(&msg.role);
                let Some(built) = self.built_message(i, self.viewport_width) else {
                    continue;
                };
                let mut segment = Segment::with_lines(Vec::new(), String::new(), Some(i));
                segment.set_kind(kind);
                install_built(&mut segment, built);
                self.cache.push(segment);
            }
        }
        self.cache.update_margins(self.viewport_width);
        self.cache.mark_built(self.messages.len());
    }

    /// The only follow state it writes is for a document with no scrollback:
    /// every row of one is on screen, so there is nowhere to be paused, and
    /// the pin has to be armed for when it grows past the viewport again.
    ///
    /// A document that does have scrollback is left alone. Shrinking under a
    /// paused reader lands them on the last row it has left, and landing
    /// there is not the statement scrolling there is: only
    /// [`Self::set_scroll_top`] and the follow controls speak for the reader.
    fn resolve_scroll(&mut self, width: u16, streaming_sum: u32, has_selection: bool) -> u32 {
        let total_lines = self.cache.total_height(width) + streaming_sum;
        self.last_total_lines = total_lines;
        let max_scroll = total_lines.saturating_sub(u32::from(self.viewport_height));
        if has_selection {
            self.scroll_top = self.scroll_top.min(max_scroll);
            return total_lines;
        }
        self.auto_scroll |= max_scroll == 0;
        self.scroll_top = if self.auto_scroll {
            max_scroll
        } else {
            self.scroll_top.min(max_scroll)
        };
        total_lines
    }

    /// Records where the reader has come to rest, against the geometry they
    /// are resting on. Called wherever `scroll_top` settles: at the end of a
    /// frame, and on the moves the reader makes between two of them.
    fn hold_reading_position(&mut self) {
        self.held_anchor = (!self.auto_scroll).then(|| self.reading_anchor()).flatten();
    }

    /// What the first visible row is looking at. `None` when that row belongs
    /// to the streaming tail, which has no segment, or to a segment carrying
    /// neither a message nor a call to name it by.
    fn reading_anchor(&self) -> Option<ReadingAnchor> {
        let (idx, rel) = self.cache.anchor_at(self.scroll_top, self.viewport_width)?;
        let seg = self.cache.get(idx)?;
        (seg.msg_index.is_some() || seg.tool_id.is_some()).then(|| ReadingAnchor {
            msg_index: seg.msg_index,
            tool_id: seg.tool_id.clone(),
            rel,
        })
    }

    /// Puts `scroll_top` back on the row the anchor names. A segment that has
    /// since shrunk past `rel` lands on its own last row, which keeps the
    /// reader inside the content they were reading rather than throwing them
    /// past it; one that is gone entirely leaves the offset for
    /// [`Self::resolve_scroll`] to clamp.
    fn restore_reading_anchor(&mut self, anchor: &ReadingAnchor, width: u16) {
        let Some(idx) = self
            .cache
            .segments()
            .iter()
            .position(|seg| anchor.names(seg))
        else {
            return;
        };
        self.scroll_top = self.cache.anchor_offset((idx, anchor.rel), width);
    }

    /// Re-lays out the stale segments the viewport plus its margin reaches,
    /// keeping the topmost visible one pinned: `scroll_top` is a line offset,
    /// so re-laying out anything above the viewport would slide the content
    /// the reader is looking at.
    ///
    /// The window is walked outward from an anchor segment, counting heights
    /// only after each segment is reflowed. Document offsets move as the
    /// reflow runs, so a window expressed in them would need repeated passes
    /// to settle; this one is right the first time.
    ///
    /// Reports whether `scroll_top` still names what it named on entry, which
    /// is false only for the frame that could not re-pin. The caller needs it
    /// to tell a position the reader is at from one they were slid to.
    fn reflow_viewport(&mut self, width: u16, has_selection: bool) -> bool {
        // `resolve_scroll` only pins to the bottom when it owns the scroll, so
        // that is exactly when the viewport is the document tail.
        let pinned_to_bottom = self.auto_scroll && !has_selection;
        let anchor = (!pinned_to_bottom)
            .then(|| self.cache.anchor_at(self.scroll_top, width))
            .flatten();
        let viewport = self.viewport_height as u32;
        let margin = viewport.saturating_mul(REFLOW_MARGIN_VIEWPORTS);
        let below = viewport.saturating_add(margin);
        // Pinned to the bottom (or scrolled into the streaming tail): the
        // viewport is the last `below` lines, so the window is all above.
        //
        // Anchored, the first visible row sits `rel` rows into the anchor
        // segment, so the downward window has to clear those before it starts
        // covering the viewport; counting from the segment's start instead
        // leaves stale segments on screen whenever `rel` exceeds the margin.
        let (start, above, below) = match anchor {
            Some((i, rel)) => (i, margin, below.saturating_add(rel as u32)),
            None => (self.cache.len().saturating_sub(1), below, below),
        };
        let len_before = self.cache.len();

        let mut acc = 0;
        let mut i = start;
        while i < self.cache.len() && acc < below {
            acc += self.reflowed_height(i, width);
            i += 1;
        }
        let mut acc = 0;
        let mut i = start;
        while i > 0 && acc < above {
            i -= 1;
            acc += self.reflowed_height(i, width);
        }

        // A rebuild can insert a missing instruction segment, which shifts the
        // anchor index. Rare enough to just skip the pin for one frame.
        let Some(anchor) = anchor else {
            return true;
        };
        if self.cache.len() != len_before {
            return false;
        }
        self.scroll_top = self.cache.anchor_offset(anchor, width);
        true
    }

    /// Reflows `seg_idx` if it is stale, colours any command it drew plain,
    /// then reports the height it draws at.
    fn reflowed_height(&mut self, seg_idx: usize, width: u16) -> u32 {
        // A tool segment and its instruction segment both map back to the
        // same parent, and one `rebuild_tool_segment` clears both flags.
        // Re-check so the parent is not rebuilt twice.
        let Some(seg) = self.cache.get_mut(seg_idx) else {
            return 0;
        };
        if seg.stale {
            self.reflow_segment(seg_idx, width);
        } else if seg.uncoloured && highlight::is_ready() {
            // Cleared up front for the reason `reflow_segment` gives.
            seg.uncoloured = false;
            if let Some(tool_id) = seg.tool_id.clone() {
                self.rebuild_tool_lines(&tool_id);
            }
        }
        self.cache
            .get(seg_idx)
            .map_or(0, |s| s.height(width) as u32)
    }

    fn reflow_segment(&mut self, seg_idx: usize, width: u16) {
        // Clear up front so a reflow that bails early (message gone, empty
        // instructions) costs one frame of old-width lines instead of
        // retrying forever.
        let Some(seg) = self.cache.get_mut(seg_idx) else {
            return;
        };
        seg.stale = false;

        if let Some(tid) = seg.tool_id.clone() {
            let parent = segment::instruction_parent(&tid)
                .map(str::to_string)
                .unwrap_or(tid);
            self.rebuild_tool_segment(&parent);
            return;
        }
        // A folded block's geometry is width-independent, but `width_changed`
        // also fires on theme changes, so it is rebuilt all the same to pick
        // up the new palette.
        self.reflow_text_segment(seg_idx, width);
    }

    fn reflow_text_segment(&mut self, seg_idx: usize, width: u16) {
        let Some(msg_idx) = self.cache.get(seg_idx).and_then(|s| s.msg_index) else {
            return;
        };
        let Some(built) = self.built_message(msg_idx, width) else {
            return;
        };
        if let Some(seg) = self.cache.get_mut(seg_idx) {
            install_built(seg, built);
        }
    }

    /// Pans for one message, indexed by diagram id. Empty when nothing in
    /// the message has been panned, which is the usual case.
    fn pans_for(&self, msg_index: usize) -> Vec<u16> {
        let highest = self
            .diagram_pans
            .keys()
            .filter(|key| key.msg_index == msg_index)
            .map(|key| key.id)
            .max();
        let Some(highest) = highest else {
            return Vec::new();
        };
        (0..=highest)
            .map(|id| {
                self.diagram_pans
                    .get(&DiagramKey { msg_index, id })
                    .copied()
                    .unwrap_or(0)
            })
            .collect()
    }
}

fn batch_entry_mut<'a>(
    output: &'a mut ToolOutput,
    indices: &str,
) -> Option<&'a mut BatchToolEntry> {
    let ToolOutput::Batch { entries, .. } = output else {
        return None;
    };
    let (index, rest) = indices
        .split_once(':')
        .map_or((indices, None), |(index, rest)| (index, Some(rest)));
    let entry = entries.get_mut(index.parse::<usize>().ok()?)?;
    match rest {
        Some(rest) => batch_entry_mut(entry.output.as_mut()?, rest),
        None => Some(entry),
    }
}

fn merge_batch_snapshot(msg: &mut DisplayMessage, mut incoming: Vec<BatchToolEntry>, text: String) {
    if let Some(ToolOutput::Batch { entries, .. }) = msg.tool_output.as_deref() {
        for (index, entry) in entries.iter().enumerate() {
            if let Some(next) = incoming.get_mut(index) {
                if entry.status.is_terminal()
                    || (entry.status == BatchToolStatus::Running && !next.status.is_terminal())
                {
                    let mut preserved = entry.clone();
                    if !entry.status.is_terminal() && next.status == BatchToolStatus::Running {
                        preserved.input = preserved.input.or_else(|| next.input.take());
                        preserved.raw_input = preserved.raw_input.or_else(|| next.raw_input.take());
                        preserved.output = preserved.output.or_else(|| next.output.take());
                        preserved.annotation =
                            preserved.annotation.or_else(|| next.annotation.take());
                    }
                    *next = preserved;
                } else if next.status < entry.status {
                    // A row the reader has watched move on keeps its place:
                    // a roster streamed before the move says less than the
                    // move did.
                    next.status = entry.status;
                }
            } else {
                incoming.push(entry.clone());
            }
        }
    }
    msg.tool_output = Some(Arc::new(ToolOutput::Batch {
        entries: incoming,
        text,
    }));
}

/// What [`same_display_item`] needs two rows to share, cheap enough to index a
/// whole transcript by: a text row keys on its length, never on the text.
#[derive(PartialEq, Eq, Hash)]
enum DisplayKey<'a> {
    Tool(&'a str),
    Delivery,
    Text(Discriminant<DisplayRole>, usize),
}

fn display_key(message: &DisplayMessage) -> Option<DisplayKey<'_>> {
    match &message.role {
        DisplayRole::Tool(tool) => Some(DisplayKey::Tool(&tool.id)),
        DisplayRole::TaskDelivery(_)
        | DisplayRole::AutomationDelivery(_)
        | DisplayRole::PeerMessage(_) => Some(DisplayKey::Delivery),
        DisplayRole::User
        | DisplayRole::Assistant
        | DisplayRole::Thinking
        | DisplayRole::Notice
        | DisplayRole::Injected => Some(DisplayKey::Text(
            mem::discriminant(&message.role),
            message.text.len(),
        )),
        DisplayRole::Error | DisplayRole::Done => None,
    }
}

/// Pairs rows with the source rows that show the same item, and hands back
/// each new pairing by row index. Sources are indexed by key, so a row costs
/// one lookup however many sources there are.
///
/// A row whose source is still listed keeps it and anchors the rows around
/// it. Sourceless rows bind next, oldest first, each to the first unclaimed
/// source past the last bound row, or else to any unclaimed one: a compaction
/// draws its summary below the turns it kept while the history re-adds those
/// turns below the summary, so order alone cannot place it. Rows whose source
/// a compaction replaced bind last, to whatever is left, so an old prompt
/// cannot take the source of a new one with the same text. They walk newest
/// first, because the turns a compaction keeps are the newest it saw.
fn matched_sources(
    rows: &[DisplayMessage],
    sources: &[DisplayMessage],
) -> Vec<(usize, Option<DisplaySource>)> {
    let mut by_key: HashMap<DisplayKey, Vec<usize>> = HashMap::new();
    for (index, source) in sources.iter().enumerate() {
        if let Some(key) = display_key(source) {
            by_key.entry(key).or_default().push(index);
        }
    }
    let mut bound: Vec<Option<usize>> = rows
        .iter()
        .map(|row| {
            let source = row.source?;
            key_candidates(&by_key, row)?
                .iter()
                .copied()
                .find(|&index| sources[index].source == Some(source))
        })
        .collect();
    let mut claimed = vec![false; sources.len()];
    for &index in bound.iter().flatten() {
        claimed[index] = true;
    }
    let mut matched = Vec::new();
    let mut visit = |row: usize, last: Option<usize>, replaced: bool| {
        if bound[row].is_some() {
            return bound[row];
        }
        let message = &rows[row];
        if message.source.is_some() != replaced {
            return last;
        }
        let fits = |index: usize| !claimed[index] && same_display_item(message, &sources[index]);
        let Some(index) = key_candidates(&by_key, message)
            .and_then(|candidates| nearest_candidate(candidates, last, replaced, fits))
        else {
            return last;
        };
        claimed[index] = true;
        bound[row] = Some(index);
        matched.push((row, sources[index].source));
        Some(index)
    };
    let mut last = None;
    for row in 0..rows.len() {
        last = visit(row, last, false);
    }
    let mut last = None;
    for row in (0..rows.len()).rev() {
        last = visit(row, last, true);
    }
    matched
}

fn key_candidates<'a>(
    by_key: &'a HashMap<DisplayKey<'a>, Vec<usize>>,
    message: &'a DisplayMessage,
) -> Option<&'a [usize]> {
    by_key.get(&display_key(message)?).map(Vec::as_slice)
}

/// The first candidate `fits` accepts, walking away from `last` in the
/// direction of the pass and then wrapping around to the rest.
fn nearest_candidate(
    candidates: &[usize],
    last: Option<usize>,
    newest_first: bool,
    fits: impl Fn(usize) -> bool,
) -> Option<usize> {
    if newest_first {
        let split = last.map_or(candidates.len(), |last| {
            candidates.partition_point(|&index| index < last)
        });
        let (before, after) = candidates.split_at(split);
        before
            .iter()
            .rev()
            .chain(after.iter().rev())
            .copied()
            .find(|&index| fits(index))
    } else {
        let split = last.map_or(0, |last| candidates.partition_point(|&index| index <= last));
        let (before, after) = candidates.split_at(split);
        after
            .iter()
            .chain(before)
            .copied()
            .find(|&index| fits(index))
    }
}

fn same_display_item(left: &DisplayMessage, right: &DisplayMessage) -> bool {
    match (&left.role, &right.role) {
        (DisplayRole::Tool(left), DisplayRole::Tool(right)) => left.id == right.id,
        (DisplayRole::TaskDelivery(left), DisplayRole::TaskDelivery(right)) => left == right,
        (DisplayRole::AutomationDelivery(left), DisplayRole::AutomationDelivery(right)) => {
            left == right
        }
        (DisplayRole::PeerMessage(left), DisplayRole::PeerMessage(right)) => left == right,
        (DisplayRole::User, DisplayRole::User)
        | (DisplayRole::Assistant, DisplayRole::Assistant)
        | (DisplayRole::Thinking, DisplayRole::Thinking)
        | (DisplayRole::Notice, DisplayRole::Notice)
        | (DisplayRole::Injected, DisplayRole::Injected) => left.text == right.text,
        _ => false,
    }
}

/// The heading an injected block folds to. Each one opens with a `# Heading`
/// inside a tag, and that heading is the same string the agent keys its
/// announcements on, so the first line that is neither blank nor a tag titles
/// the row without needing a table of kinds.
fn injected_title(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('<'))
        .map(|line| line.trim_start_matches('#').trim())
        .filter(|line| !line.is_empty())
        .unwrap_or(INJECTED_FALLBACK_TITLE)
}

fn injected_line(text: &str) -> Vec<Line<'static>> {
    let style = notice_style();
    vec![Line::from(vec![
        Span::styled(style.prefix, style.prefix_style),
        Span::styled(injected_title(text).to_owned(), style.text_style),
    ])]
}

fn thought_line(title: Option<&str>, duration: Option<Duration>, done: bool) -> Vec<Line<'static>> {
    let theme = theme::current();
    let label = if done { THOUGHT_PREFIX } else { "Thinking" };
    let header_style = if done {
        theme.thinking
    } else {
        theme.todo_in_progress
    };
    let mut spans = Vec::new();
    if !done && let Some(duration) = duration {
        spans.push(Span::styled(
            spinner_str(duration.as_millis()),
            theme.spinner,
        ));
    }
    spans.push(Span::styled(label, header_style));
    if let Some(title) = title {
        spans.push(Span::styled(format!(": {title}"), header_style));
    }
    if let Some(duration) = duration {
        spans.push(Span::styled(
            format!(
                " · {}",
                if done {
                    format_settled_duration(duration)
                } else {
                    format_live_duration(duration)
                }
            ),
            theme.tool_dim,
        ));
    }
    vec![Line::from(spans)]
}

fn segment_kind(role: &DisplayRole) -> SegmentKind {
    match role {
        DisplayRole::User => SegmentKind::User,
        DisplayRole::Assistant => SegmentKind::Assistant,
        DisplayRole::Thinking => SegmentKind::Thinking,
        DisplayRole::Error => SegmentKind::Error,
        DisplayRole::Done => SegmentKind::Done,
        DisplayRole::Notice | DisplayRole::Injected => SegmentKind::Assistant,
        DisplayRole::TaskDelivery(_) => SegmentKind::TaskDelivery,
        DisplayRole::AutomationDelivery(_) => SegmentKind::AutomationDelivery,
        DisplayRole::PeerMessage(_) => SegmentKind::PeerMessage,
        DisplayRole::Tool(_) => SegmentKind::ToolBlock,
    }
}

/// A call is a card in every mode; only its chrome changes. Compact and auto
/// strip the rail and the padding to keep the list dense, which left the rows
/// with nothing to separate them from the prose around them, so they keep the
/// panel background the expanded card already has. Expanded reaches this arm
/// only for a trivial one-line call, which reads as prose and stays flat.
fn segment_styles(
    kind: SegmentKind,
    accent: Color,
    compact: bool,
) -> (Option<Style>, Option<Style>) {
    let theme = theme::current();
    match kind {
        SegmentKind::User => (
            Some(theme.user_message_style()),
            Some(Style::new().fg(accent)),
        ),
        SegmentKind::ToolBlock
        | SegmentKind::Instruction
        | SegmentKind::TaskDelivery
        | SegmentKind::AutomationDelivery
        | SegmentKind::PeerMessage => {
            (Some(theme.panel_style()), Some(theme.subtle_border_style()))
        }
        SegmentKind::Error => (Some(theme.panel_style()), Some(theme.error)),
        SegmentKind::ToolInline => (compact.then(|| theme.panel_style()), None),
        SegmentKind::Assistant | SegmentKind::Thinking | SegmentKind::Done => (None, None),
    }
}

/// Builds ratatui lines for a non-Tool, non-collapsed-Thinking message at the
/// given width, returning the lines and search text. Shared by
/// `rebuild_line_cache` (new messages) and `reflow_text_segment` (stale-on-resize
/// messages) so both paths produce identical segments.
///
/// `window` is where reasoning draws its body, and is ignored for any other
/// role.
fn build_message_lines(
    msg: &DisplayMessage,
    width: u16,
    diagram_pans: Vec<u16>,
    window: Option<ScrollWindow>,
) -> BuiltMessage {
    let width = SegmentChrome::for_kind(segment_kind(&msg.role), width, 0).content_width(width);
    if matches!(msg.role, DisplayRole::Thinking) {
        return build_thinking_lines(msg, width, diagram_pans, window);
    }
    if let DisplayRole::TaskDelivery(origin) = &msg.role {
        let (lines, links) = task_card::delivery(origin, &msg.text, width);
        return BuiltMessage {
            links,
            ..BuiltMessage::bare(lines, msg.text.clone())
        };
    }
    if let DisplayRole::AutomationDelivery(origin) = &msg.role {
        let (lines, links) = automation_inspector::transcript::row(origin, &msg.text, width);
        return BuiltMessage {
            links,
            ..BuiltMessage::bare(lines, msg.text.clone())
        };
    }
    if let DisplayRole::PeerMessage(origin) = &msg.role {
        let (lines, links, search_text) = peer_card::delivery(origin, &msg.text, width);
        return BuiltMessage {
            links,
            ..BuiltMessage::bare(lines, search_text)
        };
    }
    let style = match &msg.role {
        DisplayRole::User => user_style(),
        DisplayRole::Assistant => assistant_style(),
        DisplayRole::Thinking => thinking_style(),
        DisplayRole::Error => error_style(),
        DisplayRole::Done => done_style(),
        DisplayRole::Notice | DisplayRole::Injected => notice_style(),
        DisplayRole::Tool(_)
        | DisplayRole::TaskDelivery(_)
        | DisplayRole::AutomationDelivery(_)
        | DisplayRole::PeerMessage(_) => {
            unreachable!()
        }
    };
    let prefix = if msg.plan_path.is_some() {
        ""
    } else {
        style.prefix
    };
    if let Some(notes) = matches!(msg.role, DisplayRole::User)
        .then(|| review::parse(&msg.text))
        .flatten()
    {
        let (lines, provenance, links) = review::card_lines(&notes, width, style.text_style);
        return BuiltMessage {
            search_text: review_search_text(&notes),
            links,
            lines,
            provenance: Some(Provenance::new(msg.text.as_str().into(), provenance)),
            diagrams: Vec::new(),
            scroll_span: None,
            scroll_footer_line: None,
        };
    }
    let (mut lines, mut provenance, mut diagrams, mut links) = if style.use_markdown {
        let (painted, parsed) = text_to_painted(
            &msg.text,
            prefix,
            style.text_style,
            style.prefix_style,
            width,
            style.max_line_bytes,
            diagram_pans,
        );
        (
            painted.lines,
            Some(Provenance::new(parsed, painted.provenance)),
            painted.diagrams,
            painted.links,
        )
    } else {
        let lines = plain_lines(&msg.text, prefix, style.text_style, style.prefix_style);
        let links = LinkMap::none_for(&lines);
        (lines, None, Vec::new(), links)
    };
    if let Some(pp) = &msg.plan_path {
        if !msg.text.is_empty() {
            let rule = hr_line(width, theme::current().plan_rule);
            if let Some(provenance) = provenance.as_mut() {
                provenance.prepend_chrome_line(rule.spans.len());
            }
            for diagram in &mut diagrams {
                diagram.rows = diagram.rows.start + 1..diagram.rows.end + 1;
            }
            lines.insert(0, rule.clone());
            links.rows.insert(0, vec![None; rule.spans.len()]);
            if let Some(provenance) = provenance.as_mut() {
                provenance.push_chrome_line(rule.spans.len());
            }
            lines.push(rule);
            links
                .rows
                .push(vec![None; lines.last().unwrap().spans.len()]);
        } else {
            lines.clear();
            links.rows.clear();
            provenance = None;
            diagrams.clear();
        }
        if !msg.text.is_empty() {
            lines.push(Line::from(""));
            links
                .rows
                .push(vec![None; lines.last().unwrap().spans.len()]);
            if let Some(provenance) = provenance.as_mut() {
                provenance.push_chrome_line(0);
            }
        }
        lines.push(Line::from(Span::styled(
            pp.to_owned(),
            theme::current().plan_path,
        )));
        links
            .rows
            .push(vec![None; lines.last().unwrap().spans.len()]);
        if let Some(provenance) = provenance.as_mut() {
            provenance.push_chrome_line(lines.last().unwrap().spans.len());
        }
        lines.push(Line::from(Span::styled(
            format!("{} to open in the workbench", key::OPEN_EDITOR.label),
            theme::current().tool_dim,
        )));
        links
            .rows
            .push(vec![None; lines.last().unwrap().spans.len()]);
        if let Some(provenance) = provenance.as_mut() {
            provenance.push_chrome_line(lines.last().unwrap().spans.len());
        }
    }
    let search_text = format!("{prefix}{}", msg.text);
    BuiltMessage {
        lines,
        search_text,
        provenance,
        diagrams,
        links,
        scroll_span: None,
        scroll_footer_line: None,
    }
}

/// A window breaks the body to the width itself, so each line is one row and
/// the window can cut rows rather than paragraphs. Without one the body keeps
/// the path it always took, and ratatui breaks the paragraphs as it paints.
fn build_thinking_lines(
    msg: &DisplayMessage,
    width: u16,
    diagram_pans: Vec<u16>,
    window: Option<ScrollWindow>,
) -> BuiltMessage {
    let summary = reasoning_summary(&msg.text);
    let header = thought_line(summary.title, msg.thinking_duration, true);
    let search_text = format!("{THINKING_SEARCH_PREFIX}{}", msg.text);
    if summary.body.is_empty() {
        return BuiltMessage::bare(header, search_text);
    }
    let style = thinking_style();
    let (painted, parsed) = match window {
        Some(_) => text_to_rows(summary.body, style.text_style, width, diagram_pans),
        None => text_to_painted(
            summary.body,
            "",
            style.text_style,
            style.prefix_style,
            width,
            style.max_line_bytes,
            diagram_pans,
        ),
    };
    let provenance = Provenance::new(parsed, painted.provenance);
    let body = thinking::Body {
        lines: &painted.lines,
        links: &painted.links,
        provenance: Some(&provenance),
        diagrams: &painted.diagrams,
    };
    BuiltMessage {
        search_text,
        ..thinking::assemble(header, body, window, ScrollTail::Settled)
    }
}

struct BuiltMessage {
    lines: Vec<Line<'static>>,
    search_text: String,
    provenance: Option<Provenance>,
    diagrams: Vec<DiagramSpan>,
    links: LinkMap,
    /// The window a reasoning body is drawn in, present only while it hides
    /// rows, and the footer row that says which.
    scroll_span: Option<ScrollSpan>,
    scroll_footer_line: Option<usize>,
}

impl BuiltMessage {
    /// Lines with nothing behind them: no link, no source to copy, no window.
    fn bare(lines: Vec<Line<'static>>, search_text: String) -> Self {
        Self {
            links: LinkMap::none_for(&lines),
            lines,
            search_text,
            provenance: None,
            diagrams: Vec::new(),
            scroll_span: None,
            scroll_footer_line: None,
        }
    }
}

/// Replaces everything a message's segment draws with what was just built. A
/// window rides along, so a block that stopped hiding rows drops its bar.
fn install_built(seg: &mut Segment, built: BuiltMessage) {
    seg.set_lines(built.lines);
    seg.set_links(built.links);
    seg.set_provenance(built.provenance);
    seg.set_diagrams(built.diagrams);
    seg.search_text = built.search_text;
    seg.scroll_spans = built.scroll_span.into_iter().collect();
    seg.scroll_footer_line = built.scroll_footer_line;
}

/// Search should reach what the card shows, not the tags behind it.
fn review_search_text(notes: &[review::ParsedNote]) -> String {
    notes
        .iter()
        .map(review::ParsedNote::search_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod shell_receipt_tests {
    use super::{MessagesPanel, batch_entry_mut};
    use crate::components::ToolStatus;
    use crate::components::code_view::Disclosure;
    use caudra_agent::tools::{BATCH_TOOL_NAME, SHELL_TOOL_NAME, ToolEffect};
    use caudra_agent::{
        BatchToolEntry, BatchToolStatus, BufferSnapshot, SharedBuf, SnapshotLine, TaskCard,
        ToolDoneEvent, ToolOutput, ToolStartEvent,
    };
    use caudra_config::UiConfig;
    use caudra_lua::EventHandle;
    use caudra_storage::background::JobKind;
    use serde_json::json;
    use std::sync::Arc;
    use test_case::test_case;

    const ROOT: &str = "shell-launch";
    const OUTPUT: &str = "**literal stdout**";
    const ANNOTATION: &str = "still running";
    const LATE: &str = "late output";

    fn receipt(call_id: &str, kind: JobKind) -> TaskCard {
        serde_json::from_value(json!({
            "kind": kind, "task_id": "readable-shell", "invocation_id": "invocation",
            "call_id": call_id, "root_call_id": ROOT, "label": "Print", "state": "running",
            "mode": "build", "background": true, "generation": 1, "created_at": 1, "updated_at": 2,
        }))
        .unwrap()
    }

    fn batch(output: ToolOutput) -> ToolOutput {
        ToolOutput::Batch {
            entries: vec![BatchToolEntry {
                tool: SHELL_TOOL_NAME.into(),
                effect: ToolEffect::Unknown,
                summary: "Print".into(),
                status: BatchToolStatus::Success,
                input: None,
                raw_input: None,
                output: Some(output),
                annotation: None,
                model_suffix: None,
                refused: false,
            }],
            text: String::new(),
        }
    }

    #[test_case(0; "standalone")]
    #[test_case(1; "batch_child")]
    #[test_case(2; "nested_batch_child")]
    fn shell_receipts_accept_progress_until_terminal_state(depth: usize) {
        let call_id = format!("{ROOT}{}", ":0".repeat(depth));
        let mut card = receipt(&call_id, JobKind::Shell);
        let mut output = ToolOutput::Tasks(vec![card.clone()]);
        for _ in 0..depth {
            output = batch(output);
        }
        let mut panel =
            MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
        let tool = if depth == 0 {
            SHELL_TOOL_NAME
        } else {
            BATCH_TOOL_NAME
        };
        panel.tool_start(ToolStartEvent {
            id: ROOT.into(),
            tool: tool.into(),
            effect: ToolEffect::Unknown,
            summary: "Print".into(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: None,
            output: Some(output.clone()),
        });
        let buf = Arc::new(SharedBuf::new());
        panel.register_live_buf(call_id.clone(), buf.clone());
        panel.tool_done(ToolDoneEvent {
            id: ROOT.into(),
            tool: tool.into(),
            output,
            is_error: false,
            ..ToolDoneEvent::error(ROOT.into(), "receipt")
        });
        assert!(panel.live_bufs.contains_key(&call_id));
        assert!(!panel.tool_in_progress(ROOT));
        assert!(panel.active_shell_call(&call_id));
        panel.tool_annotation(&call_id, ANNOTATION.into());
        panel.tool_output(&call_id, OUTPUT);
        let message = panel.messages.last_mut().unwrap();
        if depth == 0 {
            assert_eq!(message.annotation.as_deref(), Some(ANNOTATION));
            assert_eq!(message.live_output.as_deref(), Some(OUTPUT));
        } else {
            let indices = call_id.strip_prefix(&format!("{ROOT}:")).unwrap();
            let entry = batch_entry_mut(
                Arc::make_mut(message.tool_output.as_mut().unwrap()),
                indices,
            )
            .unwrap();
            assert_eq!(entry.annotation.as_deref(), Some(ANNOTATION));
            let (parent, _) = call_id.rsplit_once(':').unwrap();
            assert_eq!(panel.batch_child_stream(parent, 0), Some(OUTPUT));
        }
        assert!(panel.dirty_cards.contains(ROOT));
        buf.append(SnapshotLine::plain(OUTPUT.into()));
        let _ = panel.poll_live_bufs();
        if depth < 2 {
            let mut ctx = panel.rctx(tool, ROOT);
            ctx.width = 100;
            ctx.policy.expanded = true;
            ctx.policy.scroll_card_lines = 0;
            let rendered = MessagesPanel::build_tool_segment_lines(
                panel.messages.last().unwrap(),
                ToolStatus::Success,
                &ctx,
                Some(Disclosure {
                    full: true,
                    ..Disclosure::default()
                }),
            );
            let text = rendered
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains(OUTPUT), "{text}");
            assert!(!text.contains("open chat"), "{text}");
        }
        card.state = "succeeded".into();
        panel.task_card_update(card);
        assert!(!panel.active_shell_call(&call_id));
        assert!(!panel.live_bufs.contains_key(&call_id));
        panel.tool_output(&call_id, LATE);
        panel.tool_annotation(&call_id, LATE.into());
        panel.tool_snapshot(&call_id, BufferSnapshot::plain_text(LATE.into()), None);
        panel.register_live_buf(call_id.clone(), Arc::new(SharedBuf::new()));
        assert!(!panel.live_bufs.contains_key(&call_id));
        let message = panel.messages.last().unwrap();
        assert_eq!(panel.tool_card(ROOT).unwrap().1.status, ToolStatus::Success);
        assert_ne!(message.live_output.as_deref(), Some(LATE));
        assert_ne!(message.annotation.as_deref(), Some(LATE));
    }
}

#[cfg(test)]
mod retained_progress_tests {
    use super::{CardScroll, MessagesPanel, child_scroll_id, refill_plan};
    use crate::components::code_view::ScrollSpan;
    use crate::components::{ToolProgress, ToolStatus};
    use caudra_agent::tools::{BATCH_TOOL_NAME, SHELL_TOOL_NAME, TASK_TOOL_NAME, ToolEffect};
    use caudra_agent::{
        ActivityChild, BatchToolEntry, BatchToolStatus, SubagentActivity, SubagentProgress,
        ToolOutput, ToolStartEvent,
    };
    use caudra_config::UiConfig;
    use caudra_lua::EventHandle;
    use caudra_storage::view::ViewMode;
    use std::time::Duration;
    use test_case::test_case;

    const PARENT_ID: &str = "task-parent";
    const FIRST_CALL: &str = "batch-first";
    const SECOND_CALL: &str = "batch-second";
    const TASK_SUMMARY: &str = "inspect files";
    const CHILD_SUMMARY: &str = "ls";
    const TOOL_COUNT: u32 = 2;
    const HISTORY_VIEW_WIDTH: u16 = 80;
    const WHEEL_NOTCH: i32 = 1;
    const HISTORY_PREFIX: usize = 20;
    const HISTORY_WINDOW_ROWS: usize = 10;
    const HISTORY_BODY_ROWS: usize = 60;
    const PAUSED_HISTORY_OFFSET: usize = HISTORY_PREFIX + 2;
    const OUTPUT_OFFSET: usize = HISTORY_PREFIX - 2;
    const SHIFTED_HISTORY_PREFIX: usize = HISTORY_PREFIX + 5;
    const SNAPSHOT_BODY_LINES: usize = 5;

    fn history_span(history_start: Option<usize>) -> ScrollSpan {
        ScrollSpan {
            child: None,
            first: 0,
            lines: HISTORY_WINDOW_ROWS,
            extent_lines: HISTORY_WINDOW_ROWS,
            total: HISTORY_BODY_ROWS,
            offset: PAUSED_HISTORY_OFFSET,
            history_start,
        }
    }

    #[test_case(4, 2 ; "wrapped_snapshot")]
    #[test_case(2, 2 ; "unwrapped_snapshot")]
    fn wheel_consumes_one_notch_in_extent_units(painted_rows: usize, selected_lines: usize) {
        let mut panel = panel(false);
        panel.rebuild_line_cache();
        let span = ScrollSpan {
            lines: painted_rows,
            extent_lines: selected_lines,
            total: SNAPSHOT_BODY_LINES,
            offset: SNAPSHOT_BODY_LINES - selected_lines,
            ..history_span(None)
        };
        let following = CardScroll::at_offset(span.offset, &span);
        assert_eq!(following.offset, span.offset);
        assert!(following.follow);
        let index = panel.cache.find_by_tool_id(PARENT_ID).unwrap();
        panel.cache.get_mut(index).unwrap().scroll_spans = vec![span];

        assert_eq!(
            panel.move_window(PARENT_ID.into(), PARENT_ID, WHEEL_NOTCH),
            0
        );
        let scroll = &panel.card_scroll[PARENT_ID];
        assert_eq!(scroll.offset, span.offset - WHEEL_NOTCH as usize);
        assert!(!scroll.follow);
    }

    #[test_case(None ; "pure_output_keeps_its_floor")]
    #[test_case(Some(HISTORY_PREFIX) ; "history_never_refills")]
    fn only_pure_output_root_windows_offer_refill_rows(history_start: Option<usize>) {
        let panel = panel(false);
        let rctx = panel.rctx(TASK_TOOL_NAME, PARENT_ID);
        let mut lines = MessagesPanel::build_tool_segment_lines(
            &panel.messages[0],
            ToolStatus::InProgress,
            &rctx,
            None,
        );
        lines.scroll_spans = vec![history_span(history_start)];
        let expected = if history_start.is_some() {
            Vec::new()
        } else {
            vec![(None, HISTORY_WINDOW_ROWS)]
        };
        assert_eq!(refill_plan(&lines, HISTORY_WINDOW_ROWS), expected);
    }

    #[test_case(OUTPUT_OFFSET, Some(HISTORY_PREFIX), None, false ; "output")]
    #[test_case(HISTORY_PREFIX, Some(HISTORY_PREFIX), Some(HISTORY_PREFIX), false ; "boundary")]
    #[test_case(PAUSED_HISTORY_OFFSET, Some(HISTORY_PREFIX), Some(HISTORY_PREFIX), false ; "history")]
    #[test_case(HISTORY_BODY_ROWS - HISTORY_WINDOW_ROWS, Some(HISTORY_PREFIX), None, true ; "following")]
    #[test_case(PAUSED_HISTORY_OFFSET, None, None, false ; "no_history")]
    fn explicit_scroll_offsets_classify_only_paused_history(
        offset: usize,
        history_start: Option<usize>,
        expected_base: Option<usize>,
        following: bool,
    ) {
        let scroll = CardScroll::at_offset(offset, &history_span(history_start));
        assert_eq!(scroll.offset, offset);
        assert_eq!(scroll.history_base, expected_base);
        assert_eq!(scroll.follow, following);
    }

    #[test_case(Some(SHIFTED_HISTORY_PREFIX) ; "prefix_growth")]
    #[test_case(Some(OUTPUT_OFFSET) ; "prefix_shrink")]
    #[test_case(Some(HISTORY_PREFIX) ; "unchanged_prefix")]
    #[test_case(None ; "missing_history")]
    fn history_rebase_keeps_the_relative_offset_without_resuming(history_start: Option<usize>) {
        let mut scroll =
            CardScroll::at_offset(PAUSED_HISTORY_OFFSET, &history_span(Some(HISTORY_PREFIX)));
        assert_eq!(
            scroll.rebase_history(history_start),
            history_start.is_some_and(|base| base != HISTORY_PREFIX)
        );
        let base = history_start.unwrap_or(HISTORY_PREFIX);
        assert_eq!(scroll.offset, base + PAUSED_HISTORY_OFFSET - HISTORY_PREFIX);
        assert_eq!(scroll.history_base, Some(base));
        assert!(!scroll.follow);
    }

    #[test_case(OUTPUT_OFFSET ; "paused_in_output")]
    #[test_case(HISTORY_BODY_ROWS - HISTORY_WINDOW_ROWS ; "following")]
    fn history_rebase_does_not_reclassify_other_readers(offset: usize) {
        let mut scroll = CardScroll::at_offset(offset, &history_span(Some(HISTORY_PREFIX)));
        let following = scroll.follow;
        assert!(!scroll.rebase_history(Some(0)));
        assert_eq!(scroll.offset, offset);
        assert_eq!(scroll.history_base, None);
        assert_eq!(scroll.follow, following);
    }

    #[test_case(None ; "root")]
    #[test_case(Some(0) ; "child")]
    fn history_rebase_matches_only_the_named_window(child: Option<usize>) {
        let mut panel = panel(child.is_some());
        let key = child.map_or_else(
            || PARENT_ID.to_owned(),
            |index| child_scroll_id(PARENT_ID, index),
        );
        let untouched_key = child_scroll_id(PARENT_ID, 1);
        let scroll =
            CardScroll::at_offset(PAUSED_HISTORY_OFFSET, &history_span(Some(HISTORY_PREFIX)));
        panel.card_scroll.insert(key.clone(), scroll);
        panel.card_scroll.insert(untouched_key.clone(), scroll);
        assert!(!panel.rebase_history_windows(PARENT_ID, &[]));
        assert_eq!(panel.card_scroll[&key].history_base, Some(HISTORY_PREFIX));

        let span = ScrollSpan {
            child,
            ..history_span(Some(SHIFTED_HISTORY_PREFIX))
        };
        assert!(panel.rebase_history_windows(PARENT_ID, &[span]));
        assert!(!panel.rebase_history_windows(PARENT_ID, &[span]));
        assert_eq!(
            panel.card_scroll[&key].offset,
            SHIFTED_HISTORY_PREFIX + PAUSED_HISTORY_OFFSET - HISTORY_PREFIX
        );
        assert_eq!(
            panel.card_scroll[&key].history_base,
            Some(SHIFTED_HISTORY_PREFIX)
        );
        assert!(!panel.card_scroll[&key].follow);
        assert_eq!(
            panel.card_scroll[&untouched_key].offset,
            PAUSED_HISTORY_OFFSET
        );
        assert_eq!(
            panel.card_scroll[&untouched_key].history_base,
            Some(HISTORY_PREFIX)
        );
    }

    fn panel(child: bool) -> MessagesPanel {
        let mut panel =
            MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
        panel.tool_start(ToolStartEvent {
            id: PARENT_ID.into(),
            tool: if child {
                BATCH_TOOL_NAME
            } else {
                TASK_TOOL_NAME
            }
            .into(),
            effect: ToolEffect::Orchestrator,
            summary: TASK_SUMMARY.into(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: None,
            output: child.then(|| ToolOutput::Batch {
                entries: vec![BatchToolEntry {
                    tool: TASK_TOOL_NAME.into(),
                    effect: ToolEffect::Orchestrator,
                    summary: TASK_SUMMARY.into(),
                    status: BatchToolStatus::Running,
                    input: None,
                    raw_input: None,
                    output: None,
                    annotation: None,
                    model_suffix: None,
                    refused: false,
                }],
                text: String::new(),
            }),
        });
        panel
    }

    fn report(id: &str) -> SubagentProgress {
        SubagentProgress {
            activity: SubagentActivity::batch(
                BATCH_TOOL_NAME.into(),
                TASK_SUMMARY,
                vec![ActivityChild {
                    tool: SHELL_TOOL_NAME.into(),
                    summary: CHILD_SUMMARY.into(),
                    status: BatchToolStatus::Running,
                }],
            )
            .with_call_id(id),
            tools: TOOL_COUNT,
            elapsed: Duration::ZERO,
        }
    }

    fn ingest(panel: &mut MessagesPanel, child: bool, report: SubagentProgress) {
        if child {
            panel.set_batch_child_progress(PARENT_ID, 0, report);
        } else {
            panel.set_tool_progress(PARENT_ID, report);
        }
    }

    fn progress(panel: &MessagesPanel, child: bool) -> &ToolProgress {
        if child {
            &panel.batch_child_progress[PARENT_ID][&0]
        } else {
            panel.messages[0].progress.as_ref().unwrap()
        }
    }

    #[test_case(false ; "wheel")]
    #[test_case(true ; "drag")]
    fn a_root_history_only_window_uses_its_published_extent(drag: bool) {
        let mut panel = panel(false);
        panel.set_view(ViewMode::Expanded);
        panel.viewport_width = HISTORY_VIEW_WIDTH;
        for call in 0..=panel.policy.scroll_card_lines {
            panel.set_tool_progress(PARENT_ID, report(&call.to_string()));
        }
        panel.rebuild_line_cache();

        let message = &panel.messages[0];
        assert!(message.render_snapshot.is_none());
        assert!(message.live_output.is_none());
        assert!(message.live_body.is_none());
        assert!(message.tool_output.is_none());
        assert!(!message.text.contains('\n'));
        let segment = panel
            .cache
            .get(panel.cache.find_by_tool_id(PARENT_ID).unwrap())
            .unwrap();
        let span = segment
            .scroll_spans
            .iter()
            .find(|span| span.child.is_none())
            .unwrap();
        assert!(span.total > span.lines);
        let max_offset = span.total - span.extent_lines;
        let history_start = span.history_start;
        assert_eq!(
            panel.window_body(PARENT_ID),
            Some((span.total, PARENT_ID.into()))
        );
        panel.card_scroll.insert(
            PARENT_ID.into(),
            CardScroll {
                offset: usize::MAX,
                follow: false,
                history_base: None,
            },
        );

        let expected_offset = if drag {
            panel.jump_window(PARENT_ID, 0);
            0
        } else {
            assert_eq!(panel.scroll_window(PARENT_ID, WHEEL_NOTCH), 0);
            max_offset - WHEEL_NOTCH as usize
        };
        let scroll = &panel.card_scroll[PARENT_ID];
        assert_eq!(scroll.offset, expected_offset);
        assert_eq!(scroll.history_base, history_start);
        assert!(!scroll.follow);
    }

    #[test_case(false ; "standalone_task")]
    #[test_case(true ; "batch_child_task")]
    fn ingestion_retains_history_until_cancellation(child: bool) {
        let mut panel = panel(child);
        let first = report(FIRST_CALL);
        let second = report(SECOND_CALL);
        ingest(&mut panel, child, first.clone());
        ingest(&mut panel, child, first.clone());
        assert_eq!(progress(&panel, child).activities().count(), 1);
        let thinking = SubagentProgress {
            activity: SubagentActivity::Thinking { title: None },
            ..first.clone()
        };
        ingest(&mut panel, child, thinking.clone());
        ingest(&mut panel, child, second.clone());

        assert_eq!(
            progress(&panel, child).activities().collect::<Vec<_>>(),
            [
                (&first.activity, false),
                (&thinking.activity, false),
                (&second.activity, true),
            ]
        );
        let key = if child {
            child_scroll_id(PARENT_ID, 0)
        } else {
            PARENT_ID.into()
        };
        panel.card_scroll.insert(
            key.clone(),
            CardScroll::at_offset(PAUSED_HISTORY_OFFSET, &history_span(Some(HISTORY_PREFIX))),
        );
        panel.cancel_in_progress();

        assert_eq!(panel.card_scroll[&key].history_base, None);
        let settled = progress(&panel, child);
        assert!(!settled.is_live());
        assert!(!settled.has_history());
        assert_eq!(settled.report.activity, second.activity);
        assert_eq!(settled.report.tools, second.tools);
        ingest(&mut panel, child, first);
        assert!(!progress(&panel, child).has_history());
        assert_eq!(progress(&panel, child).report.activity, second.activity);
    }
}
