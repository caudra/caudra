mod layout;
mod render;
mod segment;
mod selection;
#[cfg(test)]
mod tests;

use self::render::{EXPAND_AFFORDANCE, HoverFeedback, RenderCursor, RenderFeedback};
use self::segment::{Segment, SegmentCache, wrapped_line_count};
use layout::{SegmentChrome, SegmentKind};

use super::tool_display::{
    RenderCtx, ToolLines, append_annotation, append_right_info, assistant_style,
    build_instructions_lines, build_tool_lines, done_style, error_style, format_timestamp_now,
    thinking_style, truncate_to_header, user_style,
};
use super::{
    DisplayMessage, DisplayRole, DisplaySource, LiveBody, ToolProgress, ToolRole, ToolStatus,
    apply_scroll_delta,
    code_view::{BatchProgressMap, BatchViewMap, Disclosure, RowTarget},
    review,
};
use crate::animation::spinner_str;
use crate::components::keybindings::key;
use crate::markdown::{
    DiagramSpan, LinkMap, TerminalLink, hr_line, plain_lines, text_to_painted, truncate_output,
    truncate_output_tail,
};
use crate::provenance::{LineProvenance, Provenance};
use crate::render_worker::RenderWorker;
use crate::selection::Selection;
use crate::splash::{ColorTransition, Splash};
use crate::theme;
use crate::update;
use caudra_config::{ClockFormat, ToolOutputLines, UiConfig};
use caudra_markdown::render::SpanSource;

use std::collections::{HashMap, HashSet, VecDeque};
use std::mem;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::scrollbar::render_vertical_scrollbar;
use super::streaming_content::StreamingContent;
use caudra_agent::tools::ToolEffect;
use caudra_agent::types::{ToolBodyDelta, ToolBodyField};
use caudra_agent::{
    BatchToolEntry, BufferSnapshot, EventSender, InstructionBlock, NO_FILES_FOUND, SharedBuf,
    SubagentProgress, ToolDoneEvent, ToolOutput, ToolStartEvent, format_live_duration,
    reasoning_summary,
};
use caudra_lua::{EventHandle, WARM_TOOL_CAP, WinView};
use caudra_storage::view::ViewMode;

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::repaint::{Cadence, Dirty};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use tracing::warn;

const THOUGHT_PREFIX: &str = "Thought";
const MILLIS_PER_SECOND: u128 = 1_000;
const SECONDS_PER_MINUTE: u64 = 60;
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
const MATH_FENCE: &str = "$$";
const MATH_BRACKET_CLOSE: &str = "\\]";
const MATH_BRACKET_OPEN: &str = "\\[";

#[derive(Debug, PartialEq, Eq)]
enum HoverTarget {
    Link(Arc<str>),
    CachedThinking(usize),
    StreamingThinking,
    Tool { id: String, feedback: HoverFeedback },
    Diagram(DiagramKey),
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

fn fenced_text(text: &str) -> String {
    let longest_run = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let delimiter = "`".repeat(longest_run.saturating_add(1).max(3));
    let newline = if text.ends_with('\n') { "" } else { "\n" };
    format!("{delimiter}text\n{text}{newline}{delimiter}")
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

/// Grows the body a streaming call is writing. The argument the text belongs
/// to is what decides the shape, so a card needs to know nothing about which
/// tool it is drawing.
fn extend_live_body(body: &mut Option<LiveBody>, delta: ToolBodyDelta) {
    let ToolBodyDelta { field, text } = delta;
    let slot = match (field, body.get_or_insert_with(|| empty_live_body(field))) {
        (ToolBodyField::Content, LiveBody::Code(code)) => code,
        (ToolBodyField::PatchText, LiveBody::Patch(patch)) => patch,
        (ToolBodyField::OldString, LiveBody::Replace { before, .. }) => before,
        (ToolBodyField::NewString, LiveBody::Replace { after, .. }) => after,
        // One call writes one shape, so a field belonging to another shape is
        // not this call's to grow.
        _ => return,
    };
    slot.push_str(&text);
}

fn empty_live_body(field: ToolBodyField) -> LiveBody {
    match field {
        ToolBodyField::Content => LiveBody::Code(String::new()),
        ToolBodyField::PatchText => LiveBody::Patch(String::new()),
        ToolBodyField::OldString | ToolBodyField::NewString => LiveBody::Replace {
            before: String::new(),
            after: String::new(),
        },
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

#[derive(Clone, Copy)]
pub struct PromptProgress {
    pub processed: u32,
    pub total: u32,
    pub cache: u32,
}

pub struct MessagesPanel {
    messages: Vec<DisplayMessage>,
    streaming_thinking: StreamingContent,
    streaming_text: StreamingContent,
    started_at: Instant,
    scroll_top: u16,
    auto_scroll: bool,
    viewport_height: u16,
    viewport_width: u16,
    viewport_area: Rect,
    cache: SegmentCache,
    last_total_lines: u16,
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
    /// Horizontal offset per drawn diagram. Absent means unpanned, so the
    /// map stays empty for the overwhelming majority of transcripts.
    diagram_pans: HashMap<DiagramKey, u16>,
    /// Per-tool log of post-completion click rows, replayed on restore.
    lua_clicks: HashMap<String, Vec<usize>>,
    live_bufs: HashMap<String, Arc<SharedBuf>>,
    /// Bufs of finished tools we keep polling so runtime-side warm
    /// clicks stay visible. Purely local: every finished-tool click
    /// carries a restore fallback, so we never track the runtime's
    /// warm cache.
    watched_bufs: VecDeque<(String, Arc<SharedBuf>)>,
    retained_shell_outputs: VecDeque<String>,
    tool_output_lines: ToolOutputLines,
    lua_event_handle: EventHandle,
    restore_event_tx: Option<EventSender>,
    show_thinking: bool,
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
    hover: Option<HoverTarget>,
    terminal_links: Vec<TerminalLink>,
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
            ),
            streaming_text: StreamingContent::new(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
                ms,
            ),
            started_at: Instant::now(),
            scroll_top: u16::MAX,
            auto_scroll: true,
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
            diagram_pans: HashMap::new(),
            lua_clicks: HashMap::new(),
            live_bufs: HashMap::new(),
            watched_bufs: VecDeque::new(),
            retained_shell_outputs: VecDeque::new(),
            tool_output_lines: ui_config.tool_output_lines,
            lua_event_handle,
            restore_event_tx: None,
            show_thinking: ui_config.show_thinking,
            streaming_reasoning_open: None,
            thinking_started: None,
            view: ViewMode::default(),
            auto_open: None,
            pending_scroll_segment: None,
            clock_format: ui_config.clock_format,
            rebake_requested: HashMap::new(),
            prompt_progress: None,
            hover: None,
            terminal_links: Vec::new(),
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
        self.clear_hover();
        self.disclosure.clear();
        self.streaming_reasoning_open = None;
        self.auto_open = None;
        for msg in &mut self.messages {
            msg.reasoning_open = None;
        }
        // Anchor before the cache goes: the modes have wildly different
        // heights, so a raw line offset would land anywhere.
        let anchor = self
            .cache
            .anchor_at(self.scroll_top as u32, self.viewport_width);
        self.cache.clear();
        if let Some((seg_idx, _)) = anchor.filter(|_| !self.auto_scroll) {
            self.pending_scroll_segment = Some(seg_idx);
        }
    }

    /// Compact and auto both draw a call as one row of a list. Expanded gives
    /// every call a card of its own.
    fn compact(&self) -> bool {
        self.view != ViewMode::Expanded
    }

    /// The card at the end of the transcript is the one being written. Live
    /// reasoning and text draw after every settled card, so while either is
    /// running nothing settled is last.
    fn is_latest(&self, msg_index: usize) -> bool {
        self.streaming_thinking.is_empty()
            && self.streaming_text.is_empty()
            && msg_index + 1 == self.messages.len()
    }

    /// Whether the mode alone draws this call's body. A call whose body is
    /// the only record of what it did stays open in every mode: closing it
    /// would hide the change.
    fn opens_by_default(&self, role: &ToolRole, msg_index: usize) -> bool {
        if !role.is_collapsible() {
            return true;
        }
        match self.view {
            ViewMode::Expanded => true,
            ViewMode::Compact => false,
            ViewMode::Auto => self.is_latest(msg_index),
        }
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

    /// Whether a click can take this card back to its header. An expanded
    /// transcript has no header to fall to, and a call that changed something
    /// has no header worth falling to.
    fn card_can_close(&self, tool_id: &str) -> bool {
        self.compact()
            && self
                .tool_card(tool_id)
                .is_some_and(|(_, role)| role.is_collapsible())
    }

    /// `None` means header-only. A card the reader has not spoken about rests
    /// within the tool's row budget; the budget is never what a click opens to,
    /// so an asked-for card is always whole.
    fn tool_expansion(&self, tool_id: &str, open_by_default: bool) -> Option<Disclosure> {
        let full = match self.disclosure.get(tool_id) {
            Some(CardState::Closed) => return None,
            Some(CardState::Full) => true,
            None if open_by_default => false,
            None => return None,
        };
        Some(Disclosure {
            full,
            shell_raw: self.shell_raw.contains(tool_id),
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
            DisplayRole::Tool(t) => t.is_collapsible() && !self.disclosure.contains_key(&t.id),
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

    fn card_closed(&self, tool_id: &str) -> bool {
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
    }

    pub fn load_messages(&mut self, mut msgs: Vec<DisplayMessage>) {
        for msg in &mut msgs {
            msg.reasoning_open = None;
        }
        self.messages = msgs;
        self.cache.clear();
        self.auto_open = None;
        self.disclosure.clear();
        self.shell_raw.clear();
        self.batch_views.clear();
        self.batch_child_progress.clear();
        self.lua_clicks.clear();
        self.live_bufs.clear();
        self.watched_bufs.clear();
        self.retained_shell_outputs.clear();
        self.rebake_requested.clear();
        self.highlight_segment = None;
        self.streaming_reasoning_open = None;
    }

    pub fn bind_sources(&mut self, source_messages: &[DisplayMessage]) {
        let mut start = 0;
        for message in &mut self.messages {
            let Some(offset) = source_messages[start..]
                .iter()
                .position(|source| same_display_item(message, source))
            else {
                continue;
            };
            let source = &source_messages[start + offset];
            message.source = source.source;
            start += offset + 1;
            if start == source_messages.len() {
                break;
            }
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
        self.streaming_text.push(text);
    }

    pub fn tool_pending(&mut self, id: String, name: &str) {
        self.flush();
        let role = DisplayRole::Tool(Box::new(ToolRole {
            id,
            status: ToolStatus::InProgress,
            name: Arc::from(name),
            effect: ToolEffect::default(),
        }));
        let mut msg = DisplayMessage::new(role, String::new());
        msg.timestamp = Some(format_timestamp_now(self.clock_format));
        self.messages.push(msg);
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
        if let Some(header) = header {
            msg.text = header;
        }
        if let Some(size) = size {
            msg.annotation = Some(size);
        }
        self.rebuild_tool_segment(tool_id);
    }

    /// The change a still-streaming call is writing. `ToolStart` drops it for
    /// the call's real output, so it only ever fills the wait.
    pub fn tool_input_body(&mut self, tool_id: &str, deltas: Vec<ToolBodyDelta>) {
        if deltas.is_empty() {
            return;
        }
        let Some(msg) = self.find_tool_msg_mut(tool_id) else {
            return;
        };
        for delta in deltas {
            extend_live_body(&mut msg.live_body, delta);
        }
        self.rebuild_tool_segment(tool_id);
    }

    pub fn tool_start(&mut self, event: ToolStartEvent) {
        if let Some(msg) = self.find_tool_msg_mut(&event.id) {
            if let DisplayRole::Tool(t) = &mut msg.role {
                t.name = Arc::clone(&event.tool);
                t.effect = event.effect;
            }
            msg.text = event.summary;
            msg.tool_input = event.input.map(Arc::new);
            msg.tool_raw_input = event.raw_input.map(Arc::new);
            msg.tool_output = event.output.map(Arc::new);
            msg.live_body = None;
            msg.annotation = event.annotation;
            msg.render_header = event.render_header;
            self.rebuild_tool_segment(&event.id);
            return;
        }
        self.flush();
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
        self.messages.push(msg);
    }

    /// Patches one child of a running batch. The roster arrived with the
    /// batch's `ToolStart`, so an event that names an index the message does
    /// not have is from a batch that is already gone.
    pub fn batch_progress(&mut self, tool_id: &str, index: usize, entry: BatchToolEntry) {
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let Some(ToolOutput::Batch { entries, .. }) = msg.tool_output.as_deref() else {
            return;
        };
        if index >= entries.len() {
            return;
        }
        let terminal = entry.status.is_terminal();
        let mut entries = entries.clone();
        entries[index] = entry;
        msg.tool_output = Some(Arc::new(ToolOutput::Batch {
            entries,
            text: String::new(),
        }));
        // What it was doing is stale the moment it stops, but what it did is
        // the only record of work its output does not show.
        if terminal {
            self.settle_child_progress(tool_id, index);
        }
        self.rebuild_tool_segment(tool_id);
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
        if !self.batch_child_exists(tool_id, index) {
            return false;
        }
        Arc::make_mut(
            self.batch_child_progress
                .entry(tool_id.to_owned())
                .or_default(),
        )
        .insert(index, ToolProgress::live(report));
        self.rebuild_tool_segment(tool_id);
        true
    }

    fn batch_child_exists(&self, tool_id: &str, index: usize) -> bool {
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
            .and_then(|msg| match msg.tool_output.as_deref() {
                Some(ToolOutput::Batch { entries, .. }) => Some(entries.len()),
                _ => None,
            })
            .is_some_and(|len| index < len)
    }

    fn settle_child_progress(&mut self, tool_id: &str, index: usize) {
        if let Some(children) = self.batch_child_progress.get_mut(tool_id)
            && let Some(progress) = Arc::make_mut(children).get_mut(&index)
        {
            progress.settle();
        }
    }

    pub fn tool_output(&mut self, tool_id: &str, content: &str) {
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
        self.rebuild_tool_segment(tool_id);
    }

    pub fn tool_done(&mut self, event: ToolDoneEvent) {
        let retain_live_output = matches!(&event.output, ToolOutput::Shell(_));
        let had_live_buf = self.retire_live_buf(&event.id);
        // A child whose own terminal event never arrived stops here with the
        // batch, so no row is left counting against a clock that has stopped.
        if let Some(children) = self.batch_child_progress.get_mut(&event.id) {
            Arc::make_mut(children)
                .values_mut()
                .for_each(ToolProgress::settle);
        }
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
        if let Some(progress) = &mut msg.progress {
            progress.settle();
        }
        msg.live_body = None;
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
        self.update_tool(tool_id, |msg| {
            msg.progress = Some(ToolProgress::live(report));
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
            let compact = self.compact();
            let seg = self.cache.get_mut(seg_idx).unwrap();
            seg.search_text = tl.search_text.clone();
            seg.update_with_reuse(tl, &self.hl_worker, compact);
        } else {
            let compact = self.compact();
            let mut seg = Segment::with_tool(inst_id, SegmentKind::Instruction);
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
        self.rebuild_tool_segment(tool_id);
    }

    pub fn stream_reset(&mut self) {
        self.streaming_thinking.clear();
        self.streaming_text.clear();
        self.streaming_reasoning_open = None;
        self.thinking_started = None;
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
                output_ref: None,
                output_limits: None,
                model_suffix: None,
                model_output: None,
                model_output_from_ref: false,
            });
        }
    }

    pub fn cancel_in_progress(&mut self) {
        let affected_ids: Vec<String> = self
            .messages
            .iter_mut()
            .filter_map(|msg| {
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
            // The stale-run_id filter drops these tools' ToolDone events,
            // so retire their live bufs here: keeps them clickable via
            // the warm path and stops them being polled forever.
            self.retire_live_buf(id);
            self.rebuild_tool_segment(id);
        }
    }

    pub fn in_progress_count(&self) -> usize {
        self.messages
            .iter()
            .filter(
                |m| matches!(&m.role, DisplayRole::Tool(t) if t.status == ToolStatus::InProgress),
            )
            .count()
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

    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    #[cfg(test)]
    pub fn message_at(&self, index: usize) -> Option<&DisplayMessage> {
        self.messages.get(index)
    }

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
        self.prompt_progress = progress;
    }

    pub fn clear_prompt_progress(&mut self) {
        self.prompt_progress = None;
    }

    pub fn flush(&mut self) {
        self.flush_thinking();
        self.prompt_progress = None;
        if !self.streaming_text.is_empty() {
            self.messages.push(DisplayMessage::new(
                DisplayRole::Assistant,
                self.streaming_text.take_all(),
            ));
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.set_scroll_top(apply_scroll_delta(self.scroll_top, delta));
    }

    /// Always unpins, and the next `view` re-pins if this lands on the
    /// bottom line.
    pub fn set_scroll_top(&mut self, top: u16) {
        self.clear_hover();
        self.scroll_top = top.min(self.max_scroll());
        self.auto_scroll = false;
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
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        let width = self.viewport_width;
        let offset = self
            .cache
            .segments()
            .iter()
            .take(segment_index)
            .map(|s| s.height(width) as u32)
            .sum::<u32>()
            .min(u16::MAX as u32) as u16;
        self.set_scroll_top(offset);
    }

    pub fn restore_scroll(&mut self, scroll_top: u16, auto_scroll: bool) {
        self.clear_hover();
        self.scroll_top = scroll_top;
        self.auto_scroll = auto_scroll;
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
        let doc_row = (row.saturating_sub(area.y)) as u32 + self.scroll_top as u32;
        let (_, segment, start) = self.cache.segment_at_row(doc_row, self.viewport_width)?;
        let rel = u16::try_from(doc_row - start).ok()?;
        (rel >= segment.chrome(self.viewport_width).margin_top)
            .then_some(segment.tool_id.as_deref())
            .flatten()
    }

    /// The dispatched id of the batch child at `row`, which is the card's own
    /// with the child's index appended. Rebuilding it is what lets a click on
    /// a roster row reach the subagent that row dispatched, since the roster
    /// carries no ids of its own.
    pub fn dispatched_id_at(&self, row: u16, area: Rect) -> Option<String> {
        if area.height == 0 {
            return None;
        }
        let doc_row = (row.saturating_sub(area.y)) as u32 + self.scroll_top as u32;
        let (_, segment, start) = self.cache.segment_at_row(doc_row, self.viewport_width)?;
        let rel = u16::try_from(doc_row - start).ok()?;
        let tool_id = segment.tool_id.as_deref()?;
        let RowTarget(index) = segment.row_target_at(rel, self.viewport_width)?;
        Some(format!("{tool_id}:{index}"))
    }

    pub fn source_at(&self, row: u16, area: Rect) -> Option<DisplaySource> {
        if area.height == 0 || row < area.y || row >= area.bottom() {
            return None;
        }
        let doc_row = (row - area.y) as u32 + self.scroll_top as u32;
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
        self.messages.iter().rev().find_map(|message| {
            matches!(&message.role, DisplayRole::Tool(tool) if tool.id == tool_id)
                .then_some(message.source)
                .flatten()
        })
    }

    pub(crate) fn update_hover(&mut self, row: u16, col: u16, area: Rect, known_task_target: bool) {
        self.hover = self.hover_target_at(row, col, area, known_task_target);
    }

    pub(crate) fn clear_hover(&mut self) {
        self.hover = None;
    }

    pub(crate) fn hovered_link(&self) -> Option<&str> {
        match &self.hover {
            Some(HoverTarget::Link(target)) => Some(target),
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
        let doc_row = (row - area.y) as u32 + self.scroll_top as u32;
        let rel_col = col - area.x;
        if let Some((_, segment, start)) = self.cache.segment_at_row(doc_row, width) {
            let rel_row = u16::try_from(doc_row - start).ok()?;
            return segment.link_at(rel_row, rel_col, width);
        }
        self.streaming_link_at(doc_row, rel_col, width)
    }

    fn hover_target_at(
        &self,
        row: u16,
        col: u16,
        area: Rect,
        known_task_target: bool,
    ) -> Option<HoverTarget> {
        if area.height == 0
            || row < area.y
            || row >= area.bottom()
            || col < area.x
            || col >= area.right()
        {
            return None;
        }
        let width = self.viewport_width;
        let doc_row = (row - area.y) as u32 + self.scroll_top as u32;
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
                .is_some_and(|message| {
                    matches!(message.role, DisplayRole::Thinking) && !self.reasoning_open(message)
                })
                .then_some(HoverTarget::CachedThinking(msg_index));
        };

        // A compact snapshot tool has not reached Lua yet, so the first click
        // is served locally and the row has to advertise itself.
        let native_toggle = (!self.has_snapshot(tool_id) || self.card_closed(tool_id))
            && self.tool_click_acts(tool_id, segment.truncation);
        let shell_toggle = segment
            .shell_toggle_line
            .is_some_and(|line| segment.source_line_at(rel, width) == Some(line));
        let batch_row = segment
            .row_target_at(rel, width)
            .and_then(|_| segment.source_line_at(rel, width));
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
            | Some(HoverTarget::Link(_))
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
            (&self.streaming_text, false, SegmentKind::Assistant),
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
                let (lines, links) = self.build_streaming_expanded_lines();
                let height = wrapped_line_count(&lines, content_width) as u32;
                if (block_start..block_start + height).contains(&doc_row) {
                    let row = u16::try_from(doc_row - block_start).ok()?;
                    let col = col.checked_sub(chrome.left)?;
                    return links.target_at(&lines, content_width, row, col);
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
            let doc_row = self.scroll_top as u32 + offset as u32;
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

    pub fn handle_click(&mut self, row: u16, area: Rect) -> bool {
        if area.height == 0 {
            return false;
        }
        self.clear_hover();
        let doc_row = (row.saturating_sub(area.y)) as u32 + self.scroll_top as u32;
        let width = self.viewport_width;
        // Both fallbacks toggle thinking: a row past the cached segments
        // belongs to the still-streaming indicator, and a segment without a
        // tool_id is a finished message's text.
        let Some((_, seg, seg_start)) = self.cache.segment_at_row(doc_row, width) else {
            return self.try_toggle_collapsed_thinking(doc_row, width);
        };
        let rel = u16::try_from(doc_row - seg_start).unwrap_or(u16::MAX);
        if rel < seg.chrome(width).margin_top {
            return false;
        }
        let Some(tool_id) = seg.tool_id.as_deref() else {
            let msg_idx = seg.msg_index;
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
        let shell_toggle = seg
            .shell_toggle_line
            .is_some_and(|line| seg.source_line_at(rel, width) == Some(line));
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
        if let Some(RowTarget(index)) = seg.row_target_at(rel, width) {
            let tool_id = tool_id.to_owned();
            self.toggle_batch_child(&tool_id, index);
            return true;
        }
        let tool_id = tool_id.to_owned();
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
            dirty |= self.idle_splash.poll_update(update::latest_version());
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
            Cadence::when(self.in_progress_count() > 0, Cadence::SPINNER),
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
    fn reasoning_open(&self, msg: &DisplayMessage) -> bool {
        msg.reasoning_open.unwrap_or(self.show_thinking)
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

    pub fn view(&mut self, frame: &mut Frame, area: Rect, has_selection: bool) {
        self.terminal_links.clear();
        if self.viewport_area != area {
            self.clear_hover();
            self.viewport_area = area;
        }
        let previous_scroll_top = self.scroll_top;
        let previous_total_lines = self.last_total_lines;
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
            let assistant = assistant_style();
            self.streaming_thinking
                .set_style("", thinking.text_style, thinking.prefix_style);
            self.streaming_text.set_style(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
            );
        }
        self.follow_latest();
        self.rebuild_line_cache();
        if let Some(seg_idx) = self.pending_scroll_segment.take() {
            self.scroll_to_segment(seg_idx.min(self.cache.len().saturating_sub(1)));
        }
        if self.in_progress_count() > 0 {
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
            let visible = self.streaming_thinking.visible().to_owned();
            let body = reasoning_summary(&visible).body.to_owned();
            if self
                .streaming_thinking
                .update_render_from(body, content_width)
            {
                self.clear_hover();
            }
            expanded_thinking = Some(self.build_streaming_expanded_lines());
            let lines = &expanded_thinking.as_ref().unwrap().0;
            if cached_count > 0 {
                streaming_heights.push(1);
            }
            streaming_heights.push(wrapped_line_count(lines, content_width));
        }

        if !self.streaming_text.is_empty() {
            let content_width =
                SegmentChrome::for_kind(SegmentKind::Assistant, width, 0).content_width(width);
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
        self.resolve_scroll(width, streaming_sum, has_selection);
        self.reflow_viewport(width, has_selection);
        self.cache.update_margins(width);
        let total_lines = self.resolve_scroll(width, streaming_sum, has_selection);
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
        let compact = self.compact();
        for (i, seg) in self.cache.segments().iter().enumerate() {
            if cursor.past_bottom() {
                break;
            }
            let highlight = self.highlight_segment == Some(i);
            let hover = self
                .hover_feedback_for_segment(seg)
                .map(|feedback| (feedback, accent));
            cursor.render(
                (seg.lines(), Some(seg.links())),
                seg.height(width),
                seg.chrome(width),
                segment_styles(seg.kind(), accent, compact),
                RenderFeedback { highlight, hover },
                frame,
            );
        }

        let mut height_idx = 0usize;
        let streamed: [(&StreamingContent, bool, SegmentKind); 2] = [
            (
                &self.streaming_thinking,
                thinking_collapsed,
                SegmentKind::Thinking,
            ),
            (&self.streaming_text, false, SegmentKind::Assistant),
        ];
        for (sc, collapsed, kind) in streamed {
            if sc.is_empty() || height_idx >= streaming_heights.len() || cursor.past_bottom() {
                continue;
            }
            if cached_count > 0 || height_idx > 0 {
                let h = streaming_heights[height_idx];
                height_idx += 1;
                cursor.render(
                    (&spacer_lines, None),
                    h,
                    SegmentChrome::for_kind(SegmentKind::Assistant, width, 0),
                    (None, None),
                    RenderFeedback::default(),
                    frame,
                );
            }
            if height_idx < streaming_heights.len() {
                let h = streaming_heights[height_idx];
                height_idx += 1;
                if collapsed {
                    let hover = matches!(self.hover, Some(HoverTarget::StreamingThinking))
                        .then_some((HoverFeedback::Chrome, accent));
                    cursor.render(
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
                } else {
                    if kind == SegmentKind::Thinking {
                        let (lines, links) = expanded_thinking.as_ref().unwrap();
                        cursor.render(
                            (lines, Some(links)),
                            h,
                            SegmentChrome::for_kind(kind, width, 0),
                            (None, None),
                            RenderFeedback::default(),
                            frame,
                        );
                    } else {
                        cursor.render(
                            (sc.cached_lines(), Some(sc.links())),
                            h,
                            SegmentChrome::for_kind(kind, width, 0),
                            (None, None),
                            RenderFeedback::default(),
                            frame,
                        );
                    }
                }
            }
        }
        self.terminal_links = cursor.into_terminal_links();

        if let Some(pp) = self.prompt_progress
            && pp.total > 0
        {
            let ratio = pp.processed as f64 / pp.total as f64;
            let bar_width = (width as f64 * 0.1).round() as u16;
            let label = " Processing ";
            let label_width = label.len() as u16;
            let total_width = label_width + bar_width;
            let bar_x = area.x + width.saturating_sub(total_width);
            let bar_y = area.y + area.height.saturating_sub(1);
            let bar_area = Rect::new(bar_x, bar_y, total_width, 1);
            crate::components::progress_bar::render(
                frame,
                bar_area,
                &crate::components::progress_bar::ProgressBarConfig {
                    ratio,
                    style: theme::current().progress_bar,
                    cache_ratio: pp.cache as f64 / pp.total as f64,
                    cache_style: Style::new().fg(Color::Green),
                    label: Some(label),
                    label_style: Some(theme::current().tool_dim),
                    bar_width,
                },
            );
            self.terminal_links
                .retain(|link| !bar_area.contains(link.position));
        }

        if total_lines > area.height {
            render_vertical_scrollbar(frame, area, total_lines, self.scroll_top);
        }
    }

    fn max_scroll(&self) -> u16 {
        self.last_total_lines.saturating_sub(self.viewport_height)
    }

    pub fn scroll_top(&self) -> u16 {
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
            let collapsed = self.streaming_thinking_collapsed();
            let (lines, provenance) = if collapsed {
                (self.build_streaming_collapsed_lines(), None)
            } else {
                let (lines, _) = self.build_streaming_expanded_lines();
                let mut provenance = if reasoning_summary(self.streaming_thinking.visible())
                    .body
                    .is_empty()
                {
                    None
                } else {
                    self.streaming_thinking.provenance().cloned()
                };
                if let Some(provenance) = provenance.as_mut() {
                    provenance.prepend_chrome_line(0);
                    provenance.prepend_chrome_line(lines[0].spans.len());
                }
                (lines, provenance)
            };
            let mut segment = Segment::with_lines(lines, String::new(), None);
            segment.set_kind(SegmentKind::Thinking);
            segment.set_margin_top(u16::from(cached_count > 0));
            segment.set_provenance(provenance);
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
        segment.set_kind(SegmentKind::Assistant);
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
                        |message| self.reasoning_open(message),
                    );
                    let reasoning = message.map_or_else(
                        || {
                            if open {
                                self.streaming_thinking.visible()
                            } else {
                                self.streaming_thinking.buffer()
                            }
                        },
                        |message| message.text.as_str(),
                    );
                    let summary = reasoning_summary(reasoning);
                    let heading = summary.title.map_or_else(
                        || "Thinking".to_owned(),
                        |title| format!("Thinking: {}", markdown_inline(title)),
                    );
                    let duration = match message {
                        Some(message) => message.thinking_duration.map(format_thought_duration),
                        None => self
                            .thinking_started
                            .map(|started| format_live_duration(started.elapsed())),
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
                    self.append_selection_section(
                        &mut document,
                        &heading,
                        Some(&metadata),
                        fragment.text.as_str(),
                        true,
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
            );
        }
        document
    }

    fn append_selection_section(
        &self,
        document: &mut String,
        heading: &str,
        metadata: Option<&str>,
        body: &str,
        fenced: bool,
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
            document.push_str(&fenced_text(body));
        } else {
            let caudra_block = unclosed_caudra_fenced_block(body);
            let commonmark_block = unclosed_markdown_block(body);
            if caudra_block != commonmark_block {
                // Conflicting parser states have no shared invisible closer.
                document.push_str(&fenced_text(&rendered_markdown_text(
                    body,
                    self.viewport_width,
                )));
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
            self.rebuild_tool_segment(tool_id);
        } else {
            warn!(
                tool_id,
                is_header, "snapshot dropped: no tool message with this id"
            );
        }
    }

    fn find_tool_msg_mut(&mut self, tool_id: &str) -> Option<&mut DisplayMessage> {
        self.messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
    }

    fn rctx(&self) -> RenderCtx<'_> {
        let kind = if self.compact() {
            SegmentKind::ToolInline
        } else {
            SegmentKind::ToolBlock
        };
        RenderCtx {
            started_at: self.started_at,
            width: SegmentChrome::for_kind(kind, self.viewport_width, 0)
                .content_width(self.viewport_width),
            tool_output_lines: &self.tool_output_lines,
            compact: self.compact(),
            batch_views: &self.batch_views,
            batch_progress: &self.batch_child_progress,
        }
    }

    pub fn register_live_buf(&mut self, id: String, body: Arc<SharedBuf>) {
        self.live_bufs.insert(id, body);
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

    fn flush_thinking(&mut self) {
        let started = self.thinking_started.take();
        if self.streaming_thinking.is_empty() {
            return;
        }
        let mut msg =
            DisplayMessage::new(DisplayRole::Thinking, self.streaming_thinking.take_all());
        msg.reasoning_open = self.streaming_reasoning_open.take();
        msg.thinking_duration = started.map(|started| started.elapsed());
        self.messages.push(msg);
    }

    fn build_streaming_collapsed_lines(&self) -> Vec<Line<'static>> {
        thought_line(
            self.streaming_thinking.buffer(),
            self.thinking_started.map(|started| started.elapsed()),
            false,
        )
    }

    fn build_streaming_expanded_lines(&self) -> (Vec<Line<'static>>, LinkMap) {
        let mut lines = thought_line(
            self.streaming_thinking.visible(),
            self.thinking_started.map(|started| started.elapsed()),
            false,
        );
        let mut links = LinkMap::none_for(&lines);
        if !self.streaming_thinking.cached_lines().is_empty() {
            lines.push(Line::from(""));
            links.rows.push(Vec::new());
            lines.extend_from_slice(self.streaming_thinking.cached_lines());
            links
                .rows
                .extend(self.streaming_thinking.links().rows.iter().cloned());
        }
        (lines, links)
    }

    fn build_cached_thinking_indicator(
        &self,
        text: &str,
        duration: Option<Duration>,
    ) -> Vec<Line<'static>> {
        thought_line(text, duration, true)
    }

    fn try_toggle_collapsed_thinking(&mut self, doc_row: u32, width: u16) -> bool {
        if !self.is_collapsed_streaming_thinking_row(doc_row, width) {
            return false;
        }
        self.streaming_reasoning_open = Some(true);
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
        if !matches!(msg.role, DisplayRole::Thinking) {
            return false;
        }
        let open = self.reasoning_open(msg);
        self.messages[idx].reasoning_open = Some(!open);
        self.rebuild_thinking_segment(idx, width);
        true
    }

    fn rebuild_thinking_segment(&mut self, msg_idx: usize, width: u16) {
        let Some(message) = self.messages.get(msg_idx).cloned() else {
            return;
        };
        let (lines, links, provenance, diagrams, search_text) = if !self.reasoning_open(&message) {
            let lines =
                self.build_cached_thinking_indicator(&message.text, message.thinking_duration);
            let links = LinkMap::none_for(&lines);
            (
                lines,
                links,
                None,
                Vec::new(),
                format!("thinking> {}", message.text),
            )
        } else {
            let built = build_message_lines(&message, width, self.pans_for(msg_idx));
            (
                built.lines,
                built.links,
                built.provenance,
                built.diagrams,
                built.search_text,
            )
        };
        let seg_idx = self
            .cache
            .segments()
            .iter()
            .position(|s| s.msg_index == Some(msg_idx) && s.tool_id.is_none());
        let Some(seg_idx) = seg_idx else { return };
        if let Some(seg) = self.cache.get_mut(seg_idx) {
            seg.set_lines(lines);
            seg.set_links(links);
            seg.set_provenance(provenance);
            seg.set_diagrams(diagrams);
            seg.search_text = search_text;
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

    /// The one row whose text is a function of the wall clock. Only a running
    /// subagent has it, and its segment is a header plus that row until the
    /// call returns, so rebuilding at the spinner cadence stays cheap.
    fn refresh_live_progress(&mut self) {
        let progress = &self.batch_child_progress;
        let live: Vec<String> = self
            .messages
            .iter()
            .filter_map(|msg| {
                let DisplayRole::Tool(tool) = &msg.role else {
                    return None;
                };
                if tool.status != ToolStatus::InProgress {
                    return None;
                }
                // A batch keeps a clock per child, since the reports belong to
                // rows the card has no header for and it carries none itself.
                let ticking = msg.progress.as_ref().is_some_and(ToolProgress::is_live)
                    || progress
                        .get(&tool.id)
                        .is_some_and(|children| children.values().any(ToolProgress::is_live));
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
                seg.apply_highlight_result(result.lines, result.rows);
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
        let msg = &self.messages[msg_idx];
        let Some(seg_idx) = self.cache.find_by_tool_id(tool_id) else {
            return;
        };

        let exp = self.tool_expansion(tool_id, opens);
        let rctx = self.rctx();
        let tl = Self::build_tool_segment_lines(msg, status, &rctx, exp);

        let instructions = msg
            .tool_output
            .as_deref()
            .and_then(|o| o.owned_instructions());

        let compact = self.compact();
        let seg = self.cache.get_mut(seg_idx).unwrap();
        seg.search_text = tl.search_text.clone();
        seg.update_with_reuse(tl, &self.hl_worker, compact);

        if let Some(blocks) = instructions {
            self.upsert_instruction_segment(tool_id, &blocks, seg_idx);
        }
        self.cache.update_margins(self.viewport_width);
    }

    fn rebuild_line_cache(&mut self) {
        if !self.cache.needs_rebuild(self.messages.len()) {
            return;
        }
        for i in self.cache.msg_count()..self.messages.len() {
            let msg = &self.messages[i];

            if let DisplayRole::Tool(t) = &msg.role {
                let exp = self.tool_expansion(&t.id, self.opens_by_default(t, i));
                let status = t.status;
                let tl = Self::build_tool_segment_lines(msg, status, &self.rctx(), exp);
                let id = t.id.clone();
                let search_text = tl.search_text.clone();
                let compact = self.compact();
                let mut seg = Segment::with_tool(id.clone(), SegmentKind::ToolBlock);
                seg.search_text = search_text;
                seg.apply_highlight(tl, &self.hl_worker, compact);
                self.cache.push(seg);

                let blocks = msg
                    .tool_output
                    .as_deref()
                    .and_then(|o| o.owned_instructions());
                if let Some(blocks) = blocks {
                    let last_idx = self.cache.len().saturating_sub(1);
                    self.upsert_instruction_segment(&id, &blocks, last_idx);
                }
            } else {
                if matches!(&msg.role, DisplayRole::Thinking) && !self.reasoning_open(msg) {
                    let (text, duration) = (msg.text.clone(), msg.thinking_duration);
                    let lines = self.build_cached_thinking_indicator(&text, duration);
                    let search_text = format!("thinking> {text}");
                    let mut segment = Segment::with_lines(lines, search_text, Some(i));
                    segment.set_links(LinkMap::none_for(segment.lines()));
                    segment.set_kind(SegmentKind::Thinking);
                    self.cache.push(segment);
                    continue;
                }
                let built = build_message_lines(msg, self.viewport_width, self.pans_for(i));
                let mut segment = Segment::with_lines(built.lines, built.search_text, Some(i));
                segment.set_kind(segment_kind(&msg.role));
                segment.set_provenance(built.provenance);
                segment.set_diagrams(built.diagrams);
                segment.set_links(built.links);
                self.cache.push(segment);
            }
        }
        self.cache.update_margins(self.viewport_width);
        self.cache.mark_built(self.messages.len());
    }

    /// Clamps `scroll_top` against the document height and applies the bottom
    /// pin, returning the total the scrollbar draws from.
    fn resolve_scroll(&mut self, width: u16, streaming_sum: u32, has_selection: bool) -> u16 {
        let total_lines: u16 =
            (self.cache.total_height(width) + streaming_sum).min(u16::MAX as u32) as u16;
        self.last_total_lines = total_lines;
        let max_scroll = total_lines.saturating_sub(self.viewport_height);
        self.scroll_top = self.scroll_top.min(max_scroll);
        if !has_selection {
            if self.scroll_top >= max_scroll {
                self.auto_scroll = true;
            }
            if self.auto_scroll {
                self.scroll_top = max_scroll;
            }
        }
        total_lines
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
    fn reflow_viewport(&mut self, width: u16, has_selection: bool) {
        // `resolve_scroll` only pins to the bottom when it owns the scroll, so
        // that is exactly when the viewport is the document tail.
        let pinned_to_bottom = self.auto_scroll && !has_selection;
        let anchor = (!pinned_to_bottom)
            .then(|| self.cache.anchor_at(self.scroll_top as u32, width))
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
        if let Some(anchor) = anchor
            && self.cache.len() == len_before
        {
            self.scroll_top = self.cache.anchor_offset(anchor, width).min(u16::MAX as u32) as u16;
        }
    }

    /// Reflows `seg_idx` if it is stale, then reports the height it draws at.
    fn reflowed_height(&mut self, seg_idx: usize, width: u16) -> u32 {
        // A tool segment and its instruction segment both map back to the
        // same parent, and one `rebuild_tool_segment` clears both flags.
        // Re-check so the parent is not rebuilt twice.
        if self.cache.get(seg_idx).is_some_and(|s| s.stale) {
            self.reflow_segment(seg_idx, width);
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
        let (tool_id, msg_idx) = (seg.tool_id.clone(), seg.msg_index);

        if let Some(tid) = tool_id {
            let parent = segment::instruction_parent(&tid)
                .map(str::to_string)
                .unwrap_or(tid);
            self.rebuild_tool_segment(&parent);
            return;
        }

        let Some(msg_idx) = msg_idx else {
            return;
        };

        let collapsed = self
            .messages
            .get(msg_idx)
            .is_some_and(|m| matches!(m.role, DisplayRole::Thinking) && !self.reasoning_open(m));
        if collapsed {
            // Geometry is width-independent, but `width_changed` also fires on
            // theme changes; rebuild so spans pick up the new palette.
            self.rebuild_thinking_segment(msg_idx, width);
        } else {
            self.reflow_text_segment(seg_idx, width);
        }
    }

    fn reflow_text_segment(&mut self, seg_idx: usize, width: u16) {
        let Some(msg_idx) = self.cache.get(seg_idx).and_then(|s| s.msg_index) else {
            return;
        };
        let Some(msg) = self.messages.get(msg_idx) else {
            return;
        };
        let built = build_message_lines(msg, width, self.pans_for(msg_idx));
        let Some(seg) = self.cache.get_mut(seg_idx) else {
            return;
        };
        seg.set_lines(built.lines);
        seg.set_provenance(built.provenance);
        seg.set_diagrams(built.diagrams);
        seg.set_links(built.links);
        seg.search_text = built.search_text;
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

fn same_display_item(left: &DisplayMessage, right: &DisplayMessage) -> bool {
    match (&left.role, &right.role) {
        (DisplayRole::Tool(left), DisplayRole::Tool(right)) => left.id == right.id,
        (DisplayRole::User, DisplayRole::User)
        | (DisplayRole::Assistant, DisplayRole::Assistant)
        | (DisplayRole::Thinking, DisplayRole::Thinking) => left.text == right.text,
        _ => false,
    }
}

fn thought_line(text: &str, duration: Option<Duration>, done: bool) -> Vec<Line<'static>> {
    let theme = theme::current();
    let summary = reasoning_summary(text);
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
    if let Some(title) = summary.title {
        spans.push(Span::styled(format!(": {title}"), header_style));
    }
    if let Some(duration) = duration {
        spans.push(Span::styled(
            format!(
                " · {}",
                if done {
                    format_thought_duration(duration)
                } else {
                    format_live_duration(duration)
                }
            ),
            theme.tool_dim,
        ));
    }
    vec![Line::from(spans)]
}

fn format_thought_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < MILLIS_PER_SECOND {
        return format!("{millis}ms");
    }
    let seconds = duration.as_secs_f64();
    if duration.as_secs() < SECONDS_PER_MINUTE {
        return format!("{seconds:.1}s");
    }
    format!(
        "{}m {}s",
        duration.as_secs() / SECONDS_PER_MINUTE,
        duration.as_secs() % SECONDS_PER_MINUTE
    )
}

fn segment_kind(role: &DisplayRole) -> SegmentKind {
    match role {
        DisplayRole::User => SegmentKind::User,
        DisplayRole::Assistant => SegmentKind::Assistant,
        DisplayRole::Thinking => SegmentKind::Thinking,
        DisplayRole::Error => SegmentKind::Error,
        DisplayRole::Done => SegmentKind::Done,
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
        SegmentKind::ToolBlock | SegmentKind::Instruction => {
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
fn build_message_lines(msg: &DisplayMessage, width: u16, diagram_pans: Vec<u16>) -> BuiltMessage {
    let width = SegmentChrome::for_kind(segment_kind(&msg.role), width, 0).content_width(width);
    if matches!(msg.role, DisplayRole::Thinking) {
        return build_thinking_lines(msg, width, diagram_pans);
    }
    let style = match &msg.role {
        DisplayRole::User => user_style(),
        DisplayRole::Assistant => assistant_style(),
        DisplayRole::Thinking => thinking_style(),
        DisplayRole::Error => error_style(),
        DisplayRole::Done => done_style(),
        DisplayRole::Tool(_) => unreachable!(),
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
            format!(
                "{} to open in editor ($VISUAL / $EDITOR)",
                key::OPEN_EDITOR.label
            ),
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
    }
}

fn build_thinking_lines(msg: &DisplayMessage, width: u16, diagram_pans: Vec<u16>) -> BuiltMessage {
    let summary = reasoning_summary(&msg.text);
    let mut lines = thought_line(&msg.text, msg.thinking_duration, true);
    let mut links = LinkMap::none_for(&lines);
    let mut provenance = None;
    let mut diagrams = Vec::new();
    if !summary.body.is_empty() {
        let style = thinking_style();
        let (painted, parsed) = text_to_painted(
            summary.body,
            "",
            style.text_style,
            style.prefix_style,
            width,
            style.max_line_bytes,
            diagram_pans,
        );
        lines.push(Line::from(""));
        links.rows.push(Vec::new());
        lines.extend(painted.lines);
        links.rows.extend(painted.links.rows);
        diagrams = painted
            .diagrams
            .into_iter()
            .map(|mut diagram| {
                diagram.rows = diagram.rows.start + 2..diagram.rows.end + 2;
                diagram
            })
            .collect();
        let mut painted_provenance = Vec::with_capacity(painted.provenance.len() + 2);
        painted_provenance.push(LineProvenance::chrome(lines[0].spans.len()));
        painted_provenance.push(LineProvenance::chrome(0));
        painted_provenance.extend(painted.provenance);
        provenance = Some(Provenance::new(parsed, painted_provenance));
    }
    BuiltMessage {
        lines,
        links,
        provenance,
        diagrams,
        search_text: format!("thinking> {}", msg.text),
    }
}

struct BuiltMessage {
    lines: Vec<Line<'static>>,
    search_text: String,
    provenance: Option<Provenance>,
    diagrams: Vec<DiagramSpan>,
    links: LinkMap,
}

/// Search should reach what the card shows, not the tags behind it.
fn review_search_text(notes: &[review::ParsedNote]) -> String {
    notes
        .iter()
        .map(review::ParsedNote::search_text)
        .collect::<Vec<_>>()
        .join("\n")
}
