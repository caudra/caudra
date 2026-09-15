use super::{DisplayMessage, ToolProgress, ToolStatus, escape_terminal_controls};

use super::code_view;
use crate::animation::{spinner_frame, spinner_str};
use crate::theme;
use caudra_config::{ClockFormat, ToolOutputLines};
use code_view::{
    BatchLiveMap, BatchProgressMap, BatchViewMap, BatchViews, BodySource, CardPolicy, Disclosure,
    RenderLimits, RowTarget, ScrollSpan, ScrollWindow, SourceTrace,
};

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write;
use std::iter;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::markdown::{
    LinkMap, should_truncate, text_to_painted, truncate_output, truncate_output_tail,
    truncation_notice,
};
use caudra_agent::{
    ActivityChild, BatchToolStatus, BufferSnapshot, InstructionBlock, NO_FILES_FOUND, ShellOutput,
    SnapshotSpan, SpanStyle, SubagentActivity, SubagentProgress, ToolInput, ToolOutput,
    tools::{
        FILE_READ_TOOL_NAME, FILE_WRITE_TOOL_NAME, PYTHON_EXECUTION_TOOL_NAME, SHELL_TOOL_NAME,
        humanize_duration,
    },
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render_worker::RenderWorker;

pub struct RenderCtx<'a> {
    pub started_at: Instant,
    pub width: u16,
    pub tool_output_lines: &'a ToolOutputLines,
    /// What the reader configured about tools: which never open, and how
    /// tall a scroll card's window is.
    pub policy: CardPolicy,
    /// This card's window, when it is drawn as a scroller, and each scrolling
    /// child's. Resolved by the panel, which owns where every window sits.
    pub card_scroll: Option<ScrollWindow>,
    pub child_scroll: Arc<HashMap<usize, ScrollWindow>>,
    pub compact: bool,
    /// How much of each batch child the reader has asked to see, by parent
    /// tool id. Looked up here rather than passed in, so every path that
    /// builds a card reads the same views the click that set them named.
    pub batch_views: &'a BatchViewMap,
    /// What each batch's dispatched children are doing, by parent tool id.
    pub batch_progress: &'a BatchProgressMap,
    /// What each batch's still-running children have streamed, by parent tool
    /// id. A child's own header does not exist, so its output is drawn from
    /// here or not at all.
    pub batch_live: &'a BatchLiveMap,
}

impl RenderCtx<'_> {
    fn limits_for(&self, tool_id: Option<&str>, full: bool, budget: usize) -> RenderLimits {
        let views = tool_id
            .and_then(|id| self.batch_views.get(id))
            .cloned()
            .unwrap_or_default();
        let progress = tool_id
            .and_then(|id| self.batch_progress.get(id))
            .cloned()
            .unwrap_or_default();
        let live = tool_id
            .and_then(|id| self.batch_live.get(id))
            .cloned()
            .unwrap_or_default();
        RenderLimits::new(full, budget, views, *self.tool_output_lines)
            .with_scroll(self.card_scroll)
            .with_policy(self.policy.clone(), self.child_scroll.clone())
            .with_progress(progress, live)
            .with_width(self.width.saturating_sub(TOOL_BODY_INDENT_WIDTH))
    }

    /// Whether this call is drawn as a fixed-height scroller rather than
    /// abridged to a budget. Turning the height down to zero gives every tool
    /// back the notice-and-click it had before.
    pub fn scrolls(&self, tool: &str) -> bool {
        self.policy.scrolls(tool)
    }

    /// The rows a card rests at, `usize::MAX` for a call with no useful
    /// abridgement.
    ///
    /// A created file is the only body that *is* its result rather than a
    /// report of one. Seven lines of it say nothing its header did not, and the
    /// notice offering the rest is on every write, so abridging it buys a click
    /// and costs the thing the card is for.
    ///
    /// Everything else a write settles into is a diff, which keeps the budget
    /// it shares with an edit and a patch. That budget is a floor there rather
    /// than a bound: a diff is already only the part that changed, so
    /// `render_tool_content` draws one whole up to its own ceiling and this
    /// number matters only when it is raised past it.
    ///
    /// A batch child is deliberately not asked: it rests at its own tool's
    /// budget so that a batch reads as the list of what it ran, and several
    /// whole files would bury that list.
    fn resting_budget(&self, tool: &str, output: Option<&ToolOutput>) -> usize {
        if self.scrolls(tool) {
            return self.policy.scroll_card_lines as usize;
        }
        if tool == FILE_WRITE_TOOL_NAME && matches!(output, Some(ToolOutput::WriteCode { .. })) {
            return usize::MAX;
        }
        self.tool_output_lines.get(tool)
    }
}

pub const TOOL_BODY_INDENT: &str = "  ";
/// Stands where the spinner does once a call has landed, so the sigil beside
/// it never changes column. Must match the width of one spinner frame plus its
/// trailing space.
const INDICATOR_PAD: &str = "  ";
const TOOL_BODY_INDENT_WIDTH: u16 = TOOL_BODY_INDENT.len() as u16;
pub(crate) const NOTICE_PREFIX: &str = "· ";
pub(crate) const SPINNER_STYLE_NAME: &str = "spinner";
pub(crate) const SPINNER_STYLE_PREFIX: &str = "spinner:";

const CODE_OUTPUT_DIVIDER: &str = "  ────────────";
/// The progress row is the only node under a card header, and `tree` draws a
/// lone child with the closing connector.
const ACTIVITY_PREFIX: &str = "  └── ";
const ACTIVITY_SEPARATOR: &str = " · ";
/// One tree level, all four columns wide so a connector and the gap below it
/// occupy the same span. Shared with `code_view` and the workflow inspector,
/// which draw the same tree: a second copy is how two surfaces drift apart.
pub(super) const TREE_BRANCH: &str = "├── ";
pub(super) const TREE_LAST: &str = "└── ";
pub(super) const TREE_TRUNK: &str = "│   ";
pub(super) const TREE_GAP: &str = "    ";
/// A window still chasing the tail, and one the reader pinned by scrolling
/// up. Named the way the log viewer names the same two states.
pub(crate) const FOLLOWING: &str = "following";
pub(crate) const PAUSED: &str = "paused";
const RESUME_HINT: &str = " · click to follow";
const SCROLL_FOOTER_SEPARATOR: &str = " · ";
pub const RAW_AFFORDANCE: &str = "click for raw";
pub const FILTERED_AFFORDANCE: &str = "click for filtered";
const COMPACT_LOAD_PREFIX: &str = "↳ Loaded ";
const COMPACT_FALLBACK_SIGIL: char = '⚙';
const COMPACT_ARG_LIMIT: usize = 3;
/// Short enough that three of them still leave the header readable, and that a
/// payload argument cannot become the row.
const COMPACT_ARG_MAX_CHARS: usize = 40;
const ELLIPSIS: char = '…';
const EDIT_KEYS: &[&str] = &["file_path", "old_string", "new_string"];
const READ_RESULT_KEYS: &[&str] = &["offset", "limit"];
/// Extensions whose file is worth more rendered than quoted.
const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "mdx"];
/// The tools whose header leads with a literal the model wrote, and the input
/// key it wrote it under. One key per tool: a grep writes a pattern, a
/// code-graph lookup writes a symbol or a whole task.
const QUERY_KEYS: &[(&str, &str)] = &[
    ("file_grep", "pattern"),
    ("file_glob", "pattern"),
    ("code_context", "task"),
    ("code_refs", "symbol"),
    ("code_impact", "symbol"),
    ("code_expand", "symbol"),
];
/// The tools whose streaming body is a script rather than a file. Both draw
/// it as numbered lines whole, so it is rendered here the way the settled card
/// renders it and never through the window their output is drawn in.
const LIVE_SCRIPT_TOOLS: &[&str] = &[SHELL_TOOL_NAME, PYTHON_EXECUTION_TOOL_NAME];
const MILLIS_PER_SECOND: u64 = 1_000;

/// Duration inputs and the millis one of their units is worth, so a bracket
/// reads `10m` instead of `600000`. `timeout` is milliseconds to a subprocess
/// and seconds to a fetch, so the unit belongs to the tool rather than to the
/// key, and reading it off the name alone would be wrong by a factor of a
/// thousand. A key that names its own unit still gets a row, because the tool
/// it belongs to is what says the name is a duration at all.
const DURATION_ARGS: &[(&str, &str, u64)] = &[
    ("shell", "timeout", 1),
    ("python_execution", "timeout", 1),
    ("webfetch", "timeout", MILLIS_PER_SECOND),
    ("websearch", "timeoutSec", MILLIS_PER_SECOND),
];
/// What separates a server or namespace from the tool it qualifies.
const QUALIFIER: [char; 3] = ['_', '.', '-'];

/// How a tool names itself on a compact row. The `name> ` prefix is gone
/// there, so the label is what identifies the call, and `header_keys` are the
/// inputs already folded into the header text so the `[k=v]` suffix can skip
/// them. Tools missing from the table fall back to their registered name.
///
/// The label is inflected, so the row says what the call is doing rather than
/// only what it is. `memory` and `sessions` keep a noun in all three slots:
/// their verb is the sub-command, which the `[k=v]` suffix already shows.
struct CompactTool {
    sigil: char,
    plain: &'static str,
    present: &'static str,
    past: &'static str,
    header_keys: &'static [&'static str],
}

/// Which form of a tool's name a row wants. The past tense asserts the call
/// happened, so anything that has not finished well uses the plain verb.
#[derive(Clone, Copy)]
pub(super) enum Tense {
    Plain,
    Present,
    Past,
}

impl CompactTool {
    fn label(&self, tense: Tense) -> &'static str {
        match tense {
            Tense::Plain => self.plain,
            Tense::Present => self.present,
            Tense::Past => self.past,
        }
    }
}

impl From<Indicator> for Tense {
    fn from(indicator: Indicator) -> Self {
        match indicator {
            Indicator::InProgress => Self::Present,
            Indicator::Success | Indicator::Warning => Self::Past,
            Indicator::Error => Self::Plain,
        }
    }
}

impl From<BatchToolStatus> for Tense {
    fn from(status: BatchToolStatus) -> Self {
        match status {
            BatchToolStatus::Running => Self::Present,
            BatchToolStatus::Success => Self::Past,
            BatchToolStatus::Pending | BatchToolStatus::Error => Self::Plain,
        }
    }
}

/// Plain, present, past — the order a verb is usually taught in. Named rather
/// than written into the table so each row stays one line, and because the
/// irregular forms are the point: a suffix rule would say "Writed" and "Runned".
type Inflection = (&'static str, &'static str, &'static str);

const READ: Inflection = ("Read", "Reading", "Read");
const FIND: Inflection = ("Find", "Finding", "Found");
const GREP: Inflection = ("Grep", "Grepping", "Grepped");
const WRITE: Inflection = ("Write", "Writing", "Wrote");
const EDIT: Inflection = ("Edit", "Editing", "Edited");
const PATCH: Inflection = ("Patch", "Patching", "Patched");
const INDEX: Inflection = ("Index", "Indexing", "Indexed");
const SEARCH: Inflection = ("Search", "Searching", "Searched");
const FETCH: Inflection = ("Fetch", "Fetching", "Fetched");
const RUN: Inflection = ("Run", "Running", "Ran");
const COMPUTE: Inflection = ("Compute", "Computing", "Computed");
const INSPECT: Inflection = ("Inspect", "Inspecting", "Inspected");
const DELEGATE: Inflection = ("Delegate", "Delegating", "Delegated");
const BATCH: Inflection = ("Batch", "Batching", "Batched");
const UPDATE: Inflection = ("Update", "Updating", "Updated");
const LOAD: Inflection = ("Load", "Loading", "Loaded");
const ASK: Inflection = ("Ask", "Asking", "Asked");
const VIEW: Inflection = ("View", "Viewing", "Viewed");
const DRAW: Inflection = ("Generate", "Generating", "Generated");
/// A store reached by sub-command. The verb is the `command` argument, which
/// the `[k=v]` suffix already shows, so the row names the store instead.
const MAP: Inflection = ("Map", "Mapping", "Mapped");
const LOCATE: Inflection = ("Locate", "Locating", "Located");
const TRACE: Inflection = ("Trace", "Tracing", "Traced");
const IMPACT: Inflection = ("Impact", "Assessing", "Assessed");
const EXPAND: Inflection = ("Expand", "Expanding", "Expanded");
const MEMORY: Inflection = ("Memory", "Memory", "Memory");
const SESSIONS: Inflection = ("Sessions", "Sessions", "Sessions");

/// Header keys are matched ignoring case and underscores, so one spelling
/// covers Workcell's camelCase wire names and the snake_case the legacy tools
/// still carry in restored sessions.
/// A sigil names its tool, because the label beside it already names the
/// operation: family-coding both spends the only per-tool identifier on what
/// the row repeats two columns right. Families survive only where the tools
/// really are one tool with several verbs — the code graph, the two stores,
/// and the cold members of read and write.
const COMPACT_TOOLS: &[(&str, CompactTool)] = &[
    tool_row("file_read", '→', READ, &["file_path"]),
    tool_row("file_glob", '✱', FIND, &["pattern", "path"]),
    tool_row("file_grep", '⌕', GREP, &["pattern", "path"]),
    tool_row("file_write", '←', WRITE, &["file_path", "content"]),
    tool_row("file_edit", '✎', EDIT, EDIT_KEYS),
    tool_row("file_apply_patch", '±', PATCH, &["patch_text"]),
    tool_row("file_index", '≡', INDEX, &["path"]),
    tool_row("websearch", '◈', SEARCH, &["query"]),
    tool_row("webfetch", '↓', FETCH, &["url"]),
    tool_row("shell", '$', RUN, &["command"]),
    tool_row("python_execution", 'λ', COMPUTE, &["code"]),
    tool_row("code_map", '◇', MAP, &["path"]),
    tool_row("code_context", '◇', LOCATE, &["task", "path"]),
    tool_row("code_refs", '◇', TRACE, &["symbol", "path"]),
    tool_row("code_impact", '◇', IMPACT, &["symbol", "path"]),
    tool_row("code_expand", '◇', EXPAND, &["symbol", "path"]),
    tool_row("execution_environment", '⌂', INSPECT, &[]),
    tool_row("task", '❖', DELEGATE, &["prompt", "description"]),
    tool_row("batch", '⇶', BATCH, &["invocations"]),
    tool_row("todo_write", '✓', UPDATE, &["todos"]),
    tool_row("skill", '→', LOAD, &["name"]),
    tool_row("question", '?', ASK, &["questions"]),
    tool_row("memory", '▤', MEMORY, &["content"]),
    tool_row("sessions", '▤', SESSIONS, &[]),
    tool_row("view_image", '→', VIEW, &["path"]),
    tool_row("image_generate", '←', DRAW, &["out", "prompt"]),
];

const fn tool_row(
    tool: &'static str,
    sigil: char,
    (plain, present, past): Inflection,
    header_keys: &'static [&'static str],
) -> (&'static str, CompactTool) {
    (
        tool,
        CompactTool {
            sigil,
            plain,
            present,
            past,
            header_keys,
        },
    )
}

/// The row a tool answers to, with the name it is tabled under. A tool
/// reaching the UI qualified still deserves its row: the same call wrapped by
/// an MCP server arrives as `mcp_File_read`, and an exact match would hand it
/// the fallback sigil, its own name in place of a label, and an empty
/// `header_keys` that repeats the whole header back in brackets. Leading
/// segments are dropped one at a time rather than the name matched as a bare
/// suffix, so the qualifier has to end where the tool name begins and an
/// unrelated `myfile_read` cannot pass for `file_read`.
fn compact_row(name: &str) -> Option<(&'static str, &'static CompactTool)> {
    qualifier_suffixes(name)
        .find_map(|rest| COMPACT_TOOLS.iter().find(|(tool, _)| same_key(tool, rest)))
        .map(|(tool, entry)| (*tool, entry))
}

/// The name, then the name with each leading qualifier segment dropped in
/// turn. Longest first, so a row matching the whole name beats one matching a
/// tail of it.
fn qualifier_suffixes(name: &str) -> impl Iterator<Item = &str> {
    iter::successors(Some(name), |rest| {
        rest.split_once(QUALIFIER).map(|(_, tail)| tail)
    })
}

/// Whether `name` is the tool `pattern` names, matched the way the compact
/// table matches its rows. Config lists a tool by its bare name, and the call
/// may still arrive qualified by the server that wrapped it.
pub(crate) fn names_tool(pattern: &str, name: &str) -> bool {
    qualifier_suffixes(name).any(|rest| same_key(pattern, rest))
}

/// What a window can say about its tail beyond the counts. `following` and
/// `paused` both claim output may still arrive, so a call that has answered
/// says neither and reports only how much sits either side of the window.
#[derive(Clone, Copy)]
pub(crate) enum ScrollTail {
    /// Still running, and this footer is the control that re-pins the window.
    Resumable,
    /// Still running, but the footer is not the control: a batch child's rows
    /// fold the child instead, so it is told where it sits and nothing more.
    Live,
    /// Answered. There is nothing left to follow.
    Settled,
}

/// What a window says about itself: how much sits either side of it, and, while
/// output can still reach it, which edge it is pinned to. `None` when the body
/// fits, since then there is no window to describe.
///
/// A card and a batch child share this so the two never drift into describing
/// the same state differently.
pub(crate) fn scroll_footer_text(above: usize, below: usize, tail: ScrollTail) -> Option<String> {
    if above == 0 && below == 0 {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if above > 0 {
        parts.push(format!("{above} above"));
    }
    if below > 0 {
        parts.push(format!("{below} below"));
    }
    match (tail, below) {
        (ScrollTail::Settled, _) => {}
        (_, 0) => parts.push(FOLLOWING.to_owned()),
        (ScrollTail::Resumable, _) => parts.push(format!("{PAUSED}{RESUME_HINT}")),
        (ScrollTail::Live, _) => parts.push(PAUSED.to_owned()),
    }
    Some(format!(
        "{NOTICE_PREFIX}{}",
        parts.join(SCROLL_FOOTER_SEPARATOR)
    ))
}

fn compact_tool(name: &str) -> Option<&'static CompactTool> {
    compact_row(name).map(|(_, entry)| entry)
}

fn query_key(name: &str) -> Option<&'static str> {
    let (tool, _) = compact_row(name)?;
    QUERY_KEYS
        .iter()
        .find_map(|(query_tool, key)| (*query_tool == tool).then_some(*key))
}

/// The activity as a row says it. A tool answers with the verb its own card
/// header uses, so a watcher reads `Reading` rather than `file_read`, and a
/// name the table has never heard of answers with itself, which is all there
/// is to say about it.
///
/// Always the present tense. The row reports what the agent is doing, or was
/// doing when it stopped, and the past tense would assert that a call finished
/// when the agent may well have been cut off mid-way through it.
pub(super) fn activity_label(activity: &SubagentActivity) -> String {
    match activity {
        SubagentActivity::Tool { name, .. } => compact_tool(name).map_or_else(
            || name.to_string(),
            |entry| entry.label(Tense::Present).to_owned(),
        ),
        phase => capitalized(phase.label()),
    }
}

/// What the activity is working on, with control characters neutralised: the
/// summary is built from tool input, which the agent does not author.
pub(super) fn activity_detail(activity: &SubagentActivity) -> Option<String> {
    activity.detail().map(escape_terminal_controls)
}

/// The sigil an activity row opens on, so it reads like the row above it and
/// the rows below it. A phase that is not a call has no tool to name, and
/// falling back to the unknown-tool sigil would assert one that never ran.
pub(super) fn activity_sigil(activity: &SubagentActivity) -> Option<char> {
    match activity {
        SubagentActivity::Tool { name, .. } => Some(compact_sigil_label(name, Tense::Present).0),
        _ => None,
    }
}

/// One child of a batch a subagent is running, drawn the way the batch card
/// draws the same call: the connector, then the sigil in its outcome colour,
/// then the tense the child's status puts the verb in.
pub(super) fn activity_child_spans(child: &ActivityChild, prefix: String) -> Vec<Span<'static>> {
    let theme = theme::current();
    let (sigil, label) = compact_sigil_label(&child.tool, child.status.into());
    let mut spans = vec![
        Span::styled(prefix, theme.tool_dim),
        Span::styled(format!("{sigil} "), batch_sigil_style(child.status, None)),
        Span::styled(label.to_owned(), theme.tool_prefix),
    ];
    if !child.summary.is_empty() {
        spans.push(Span::styled(
            format!(" {}", escape_terminal_controls(&child.summary)),
            theme.tool_dim,
        ));
    }
    spans
}

/// A phase label is authored lowercase to read mid-sentence, but on this row it
/// stands where a tool's inflected verb would and has to match it.
fn capitalized(label: &str) -> String {
    let mut rest = label.chars();
    rest.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(rest).collect()
    })
}

/// How a tool introduces itself on a one-line row. A name the table has never
/// heard of answers with itself, which is all there is to say about it.
pub(super) fn compact_sigil_label(name: &str, tense: Tense) -> (char, &str) {
    compact_tool(name).map_or((COMPACT_FALLBACK_SIGIL, name), |entry| {
        (entry.sigil, entry.label(tense))
    })
}

/// Whether a write to `path` is drawn as the document it is rather than as its
/// source. A markdown file's rendering *is* what the write produced, so the
/// gutter costs the thing the card is for.
fn renders_as_markdown(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            MARKDOWN_EXTENSIONS
                .iter()
                .any(|md| ext.eq_ignore_ascii_case(md))
        })
}

fn same_key(left: &str, right: &str) -> bool {
    let normalize = |key: &str| {
        key.chars()
            .filter(|c| *c != '_')
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    normalize(left) == normalize(right)
}

/// A search header opens with the pattern and continues `in <path>`, so the
/// text searched for and the sentence built around it arrive as one string.
/// A pattern may hold spaces, the word `in`, or regex punctuation, so where it
/// ends is not recoverable from the header; the input is what still has it
/// verbatim. Italic rather than a colour because the query is a literal, which
/// is what italic already marks everywhere else, and because a new colour would
/// have to mean something in every theme.
pub(super) fn header_spans(
    tool: &str,
    header: &str,
    base: Style,
    raw_input: Option<&serde_json::Value>,
) -> Vec<Span<'static>> {
    let query = query_key(tool)
        .and_then(|key| raw_input?.get(key)?.as_str())
        .filter(|query| !query.is_empty() && header.starts_with(query));
    let Some(query) = query else {
        return vec![Span::styled(header.to_owned(), base)];
    };
    let mut spans = vec![Span::styled(
        query.to_owned(),
        base.add_modifier(Modifier::ITALIC),
    )];
    let rest = &header[query.len()..];
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_owned(), base));
    }
    spans
}

/// The same for a tool named at runtime. A batch child knows only its tool's
/// name, so it resolves the keys its header already shows the way the row for
/// that tool would.
pub(super) fn compact_args_for(
    tool: &str,
    header: &str,
    raw_input: Option<&serde_json::Value>,
    output: Option<&ToolOutput>,
) -> Option<String> {
    let row = compact_row(tool);
    compact_args(
        raw_input,
        header,
        row.map(|(tool, _)| tool),
        row.map_or(&[], |(_, entry)| entry.header_keys),
        output,
    )
}

/// The value of a duration input in milliseconds, or `None` for a number that
/// is not one. The tool has to be one the table knows, since the unit is its
/// to declare.
fn duration_millis(tool: Option<&str>, key: &str, value: &serde_json::Number) -> Option<u64> {
    let tool = tool?;
    let (_, _, millis_per_unit) = DURATION_ARGS
        .iter()
        .find(|(known, arg, _)| *known == tool && same_key(arg, key))?;
    value.as_u64()?.checked_mul(*millis_per_unit)
}

/// The primitive inputs a compact header does not already show, rendered the
/// way opencode does: `[offset=1, limit=260]`.
///
/// `header_keys` names what a known tool folded into its header, and also the
/// inputs too big to belong on one row at all. A completed structured read
/// folds its pagination keys into the output range for the same reason. The
/// header itself is the backstop for the rest: a tool the table has never heard
/// of would otherwise print its whole header back as `[k=v]`. Only strings are
/// checked against it, because a number is what the brackets exist to carry
/// while a call has no result yet, and `offset=1` must survive a header that
/// happens to contain a `1`.
///
/// `tool` is the name the row is tabled under, which is what says whether a
/// number is a count or a duration. Every string is bounded on the way in, so
/// a tool the table has never heard of cannot spill a payload across the row.
fn compact_args(
    raw_input: Option<&serde_json::Value>,
    header: &str,
    tool: Option<&str>,
    header_keys: &[&str],
    output: Option<&ToolOutput>,
) -> Option<String> {
    let fields = raw_input?.as_object()?;
    let result_keys = match (tool, output) {
        (Some(FILE_READ_TOOL_NAME), Some(ToolOutput::ReadCode { .. })) => READ_RESULT_KEYS,
        _ => &[],
    };
    let mut rendered = String::new();
    let mut shown = 0;
    for (key, value) in fields.iter().filter(|(key, _)| {
        !header_keys
            .iter()
            .chain(result_keys)
            .any(|folded| same_key(folded, key))
    }) {
        let scalar = match value {
            serde_json::Value::String(text) if header.contains(text.as_str()) => continue,
            serde_json::Value::String(text) => one_line(text),
            serde_json::Value::Number(number) => duration_millis(tool, key, number).map_or_else(
                || number.to_string(),
                |millis| humanize_duration(Duration::from_millis(millis)),
            ),
            serde_json::Value::Bool(flag) => flag.to_string(),
            _ => continue,
        };
        if shown > 0 {
            rendered.push_str(", ");
        }
        write!(rendered, "{key}={scalar}").unwrap();
        shown += 1;
        if shown == COMPACT_ARG_LIMIT {
            break;
        }
    }
    (!rendered.is_empty()).then(|| format!(" [{rendered}]"))
}

/// One line, single-spaced, and short enough that the value cannot become the
/// row. A tabled tool folds its payload away by key, but an untabled one has
/// no `header_keys` to fold anything with, and a file body arriving under a
/// name nobody knows would otherwise be pasted across the header verbatim.
fn one_line(value: &str) -> String {
    let mut out = String::new();
    for word in value.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if let Some((cut, _)) = out.char_indices().nth(COMPACT_ARG_MAX_CHARS) {
        out.truncate(cut);
        out.push(ELLIPSIS);
    }
    out
}

pub struct RoleStyle {
    pub prefix: &'static str,
    pub text_style: Style,
    pub prefix_style: Style,
    pub use_markdown: bool,
    pub max_line_bytes: Option<usize>,
}

pub fn assistant_style() -> RoleStyle {
    RoleStyle {
        prefix: "",
        text_style: theme::current().assistant,
        prefix_style: theme::current().assistant,
        use_markdown: true,
        max_line_bytes: None,
    }
}

pub fn user_style() -> RoleStyle {
    RoleStyle {
        prefix: "",
        text_style: theme::current().assistant,
        prefix_style: theme::current().assistant,
        use_markdown: true,
        max_line_bytes: None,
    }
}

pub fn thinking_style() -> RoleStyle {
    RoleStyle {
        prefix: "thinking> ",
        text_style: theme::current().thinking,
        prefix_style: theme::current().thinking,
        use_markdown: true,
        max_line_bytes: None,
    }
}

pub fn error_style() -> RoleStyle {
    RoleStyle {
        prefix: "",
        text_style: theme::current().error,
        prefix_style: theme::current().tool_error,
        use_markdown: false,
        max_line_bytes: None,
    }
}

pub fn notice_style() -> RoleStyle {
    RoleStyle {
        prefix: NOTICE_PREFIX,
        text_style: theme::current().status_dim.add_modifier(Modifier::ITALIC),
        prefix_style: theme::current().status_dim,
        use_markdown: false,
        max_line_bytes: None,
    }
}

pub fn done_style() -> RoleStyle {
    RoleStyle {
        prefix: "",
        text_style: theme::current()
            .tool_success
            .add_modifier(ratatui::style::Modifier::BOLD),
        prefix_style: theme::current().tool_success,
        use_markdown: false,
        max_line_bytes: None,
    }
}

pub struct ToolLines {
    pub lines: Vec<Line<'static>>,
    pub links: LinkMap,
    pub search_text: String,
    pub highlight: Option<HighlightRequest>,
    pub spinner_lines: Vec<(usize, usize)>,
    /// Index of the first live-buffer snapshot line, recorded in the same
    /// pass that lays out `lines`, so click rows can never drift from them.
    pub snapshot_base: Option<usize>,
    /// Snapshot lines the card's window left above its first drawn row, so a
    /// click still names the buffer line it landed on.
    pub snapshot_skip: usize,
    pub shell_toggle_line: Option<usize>,
    /// The scroll card's footer, which re-pins the window to the tail.
    pub scroll_footer_line: Option<usize>,
    /// Every window drawn in these lines, the card's own and each scrolling
    /// child's, so a bar can be placed beside each.
    pub scroll_spans: Vec<ScrollSpan>,
    pub content_indent: &'static str,
    pub truncation: bool,
    /// What each line belongs to, parallel to `lines`, so a splice keeps the
    /// two in step and the async highlight cannot lose a click target.
    pub rows: Vec<Option<RowTarget>>,
    /// Where each line came from, parallel to `lines`, so copy slices the
    /// script instead of scraping its line-number gutter off the screen.
    /// `None` when something in the card names no source at all.
    pub source: Option<BodySource>,
}

pub struct HighlightRequest {
    pub range: (usize, usize),
    pub input: Option<Arc<ToolInput>>,
    pub output: Option<Arc<ToolOutput>>,
    pub limits: RenderLimits,
}

impl HighlightRequest {
    fn new(
        range: (usize, usize),
        input: Option<Arc<ToolInput>>,
        output: Option<Arc<ToolOutput>>,
        limits: RenderLimits,
    ) -> Option<Self> {
        if range.0 == range.1 {
            return None;
        }
        let output = output.and_then(|o| match *o {
            ToolOutput::ReadCode { .. }
            | ToolOutput::WriteCode { .. }
            | ToolOutput::Diff { .. }
            | ToolOutput::Patch { .. }
            | ToolOutput::GrepResult { .. }
            | ToolOutput::Index(_)
            | ToolOutput::CodeGraph { .. }
            | ToolOutput::Instructions { .. } => Some(o),
            ToolOutput::Plain(_)
            | ToolOutput::Markdown(_)
            | ToolOutput::ReadDir(_)
            | ToolOutput::TodoList(_)
            | ToolOutput::Answers(_)
            | ToolOutput::Shell(_)
            | ToolOutput::Environment { .. }
            | ToolOutput::WorkflowRun(_)
            | ToolOutput::Image { .. } => None,
            // Children carry their own code and diffs, so a batch reaches the
            // highlighting worker exactly as a lone child would. A batch with
            // a child still reporting or still printing is the exception: its
            // rows move faster than the worker's cache key, so an answer
            // spliced back over them would freeze what the reader is watching.
            ToolOutput::Batch { ref entries, .. } => {
                (!entries.is_empty() && !limits.has_live_rows()).then_some(o)
            }
        });
        if input.is_none() && output.is_none() {
            return None;
        }
        Some(Self {
            range,
            input,
            output,
            limits,
        })
    }
}

impl ToolLines {
    pub fn send_highlight(&self, worker: &RenderWorker) -> Option<u64> {
        let hl = self.highlight.as_ref()?;
        Some(worker.send(hl.input.clone(), hl.output.clone(), hl.limits.clone()))
    }
}

pub fn format_timestamp_now(format: ClockFormat) -> String {
    let zoned = Timestamp::now().to_zoned(TimeZone::system());
    zoned.strftime(crate::clock::hms(format)).to_string()
}

pub fn append_right_info(
    line: &mut Line<'static>,
    usage: Option<&str>,
    timestamp: Option<&str>,
    width: u16,
) {
    if usage.is_none() && timestamp.is_none() {
        return;
    }
    let separator = if usage.is_some() && timestamp.is_some() {
        2
    } else {
        0
    };
    let suffix_len =
        usage.map_or(0, UnicodeWidthStr::width) + timestamp.map_or(0, str::len) + separator + 1;
    let header_width: usize = line
        .spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let w = width as usize;
    if header_width + 1 + suffix_len > w {
        return;
    }
    let pad = w - header_width - suffix_len;
    line.spans.push(Span::raw(" ".repeat(pad)));
    if let Some(u) = usage {
        line.spans
            .push(Span::styled(u.to_owned(), theme::current().tool_dim));
        if timestamp.is_some() {
            line.spans.push(Span::raw("  "));
        }
    }
    if let Some(ts) = timestamp {
        line.spans
            .push(Span::styled(ts.to_owned(), theme::current().timestamp));
    }
}

#[derive(Clone, Copy)]
enum Indicator {
    InProgress,
    Success,
    /// The call worked and answered with nothing.
    Warning,
    Error,
}

impl From<ToolStatus> for Indicator {
    fn from(s: ToolStatus) -> Self {
        match s {
            ToolStatus::InProgress => Self::InProgress,
            ToolStatus::Success => Self::Success,
            ToolStatus::Error => Self::Error,
        }
    }
}

impl Indicator {
    /// A search that found nothing succeeded, so nothing about the status says
    /// the answer is empty, and `0 matches` reads like any other count at a
    /// glance. The output is what knows, so the color comes from there.
    fn resolve(status: ToolStatus, output: Option<&ToolOutput>) -> Self {
        match (Self::from(status), output) {
            (Self::Success, Some(output)) if found_nothing(output) => Self::Warning,
            (indicator, _) => indicator,
        }
    }
}

/// Whether a finished call is a confirmed miss. A search that stopped early is
/// not one: Workcell says so by writing its own notice instead of the empty
/// answer, so comparing against that answer excludes a capped scan by
/// construction rather than by guessing at the text.
fn found_nothing(output: &ToolOutput) -> bool {
    match output {
        ToolOutput::GrepResult { entries, .. } => {
            entries.iter().all(|entry| entry.match_count() == 0)
        }
        ToolOutput::Plain(text) => text.text == NO_FILES_FOUND,
        _ => false,
    }
}

/// The color a finished batch child answers with, resolved here so every
/// status-to-color decision lives beside the sigils it paints.
pub(super) fn batch_sigil_style(status: BatchToolStatus, output: Option<&ToolOutput>) -> Style {
    let theme = theme::current();
    match status {
        BatchToolStatus::Pending => theme.tool_dim,
        BatchToolStatus::Running => theme.spinner,
        BatchToolStatus::Success => finished_style(Indicator::resolve(ToolStatus::Success, output)),
        BatchToolStatus::Error => theme.tool_error,
    }
}

struct ResolvedOutput<'a> {
    text: Option<Cow<'a, str>>,
    full_text: Option<Cow<'a, str>>,
    skipped: usize,
    /// Lines above and below a scroll card's window. `None` for a body that
    /// was abridged to a budget instead, which reports itself with `skipped`.
    scrolled: Option<(usize, usize)>,
}

fn resolve_output<'a>(
    output: Option<&'a ToolOutput>,
    body: Option<&'a str>,
    live_output: Option<&'a str>,
    pre_truncated: usize,
    limits: RenderLimits,
    shell_raw: bool,
) -> ResolvedOutput<'a> {
    let full_text: Option<Cow<'a, str>> = match output {
        Some(ToolOutput::Plain(t) | ToolOutput::Markdown(t) | ToolOutput::ReadDir(t)) => {
            Some(Cow::Borrowed(t.text.as_str()))
        }
        // A batch with children draws them structurally; only a session from
        // when batch was a Lua plugin falls back to the model's own text.
        Some(ToolOutput::Batch { entries, text }) if entries.is_empty() => {
            Some(Cow::Borrowed(text.as_str()))
        }
        Some(ToolOutput::Shell(output)) => Some(if output.filter.is_some() && !shell_raw {
            Cow::Borrowed(output.model_text.as_str())
        } else {
            match live_output {
                Some(live) => Cow::Borrowed(live),
                None => Cow::Owned(output.raw_text()),
            }
        }),
        _ => None,
    };

    let expanded = limits.is_expanded();
    // `body` was abridged to the tool's budget on the way in, so anything
    // that means to show more than the budget has to read the output itself.
    // A window means exactly that: it is free to sit anywhere in the body.
    let whole = expanded || limits.scroll.is_some();
    let (raw_text, already_truncated): (Option<Cow<'a, str>>, usize) = if whole {
        match &full_text {
            Some(t) => (Some(t.clone()), 0),
            None if output.is_some() => {
                return ResolvedOutput {
                    text: None,
                    full_text: None,
                    skipped: 0,
                    scrolled: None,
                };
            }
            None => match live_output {
                Some(live) => (Some(Cow::Borrowed(live)), 0),
                None => match body {
                    Some(b) => (Some(Cow::Borrowed(b)), pre_truncated),
                    None => (None, 0),
                },
            },
        }
    } else {
        match (body, &full_text) {
            (Some(b), _) => (Some(Cow::Borrowed(b)), pre_truncated),
            (None, Some(t)) => (Some(t.clone()), 0),
            (None, None) if output.is_some() => {
                return ResolvedOutput {
                    text: None,
                    full_text: None,
                    skipped: 0,
                    scrolled: None,
                };
            }
            (None, None) => (None, 0),
        }
    };

    // A window is the reader's own position in the body, so it outranks both
    // the budget and the expansion a click would otherwise have granted.
    if let Some(window) = limits.scroll.filter(|_| !expanded) {
        let (text, scrolled) = match raw_text {
            Some(t) if !t.is_empty() => {
                let (kept, above, below) = window_lines(&t, window);
                (Some(Cow::Owned(kept)), Some((above, below)))
            }
            _ => (None, None),
        };
        return ResolvedOutput {
            text,
            full_text,
            skipped: 0,
            scrolled,
        };
    }

    let keep_tail = matches!(output, Some(ToolOutput::Shell(_)));
    let (text, skipped) = match raw_text {
        Some(t) if !t.is_empty() => {
            let tr = if keep_tail {
                truncate_output_tail(&t, limits.budget)
            } else {
                truncate_output(&t, limits.budget)
            };
            let s = if tr.skipped > 0 {
                tr.skipped
            } else {
                already_truncated
            };
            (Some(Cow::Owned(tr.kept.into_owned())), s)
        }
        _ => (None, already_truncated),
    };

    ResolvedOutput {
        text,
        full_text,
        skipped,
        scrolled: None,
    }
}

/// The window's slice of `text`, with the line counts either side of it.
fn window_lines(text: &str, window: ScrollWindow) -> (String, usize, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = window.range(lines.len());
    (lines[start..end].join("\n"), start, lines.len() - end)
}

struct ToolLineBuilder {
    lines: Vec<Line<'static>>,
    link_rows: Vec<(usize, Vec<Option<Arc<str>>>)>,
    search_text: String,
    spinner_lines: Vec<(usize, usize)>,
    snapshot_base: Option<usize>,
    snapshot_skip: usize,
    shell_toggle_line: Option<usize>,
    scroll_footer_line: Option<usize>,
    scroll_spans: Vec<ScrollSpan>,
    content_range: (usize, usize),
    rows: Vec<Option<RowTarget>>,
    source: SourceTrace,
    width: u16,
    truncation: bool,
    limits: RenderLimits,
    markdown: bool,
    indicator: Indicator,
    /// The card's tool, resolved once at header time so the head, a plugin's
    /// baked spinner slot, and a compact row cannot disagree about what this
    /// call is.
    sigil: char,
}

impl ToolLineBuilder {
    fn new(width: u16, indicator: Indicator, limits: RenderLimits) -> Self {
        Self {
            sigil: COMPACT_FALLBACK_SIGIL,
            lines: Vec::new(),
            link_rows: Vec::new(),
            search_text: String::new(),
            spinner_lines: Vec::new(),
            snapshot_base: None,
            snapshot_skip: 0,
            shell_toggle_line: None,
            scroll_footer_line: None,
            scroll_spans: Vec::new(),
            content_range: (0, 0),
            rows: Vec::new(),
            source: SourceTrace::default(),
            width,
            truncation: false,
            limits,
            markdown: false,
            indicator,
        }
    }

    fn apply_output_format(&mut self, output: Option<&ToolOutput>) {
        if output.is_some_and(ToolOutput::is_markdown) {
            self.markdown = true;
        }
    }

    fn push_header(
        &mut self,
        tool_name: &str,
        header: &str,
        annotation: Option<&str>,
        render_header: Option<&BufferSnapshot>,
        output: Option<&ToolOutput>,
        raw_input: Option<&serde_json::Value>,
    ) {
        let (sigil, label) = compact_sigil_label(tool_name, self.indicator.into());
        self.sigil = sigil;
        // An omitted header leaves the label against the annotation, so the
        // separator goes with the text it separates.
        let gap = if header.is_empty() { "" } else { " " };
        let mut spans = vec![Span::styled(
            format!("{label}{gap}"),
            theme::current().tool_prefix,
        )];
        if let Some(snapshot) = render_header {
            if let Some(first_line) = snapshot.lines.first() {
                let line_idx = self.lines.len();
                let spinners = &mut self.spinner_lines;
                bake_spans(
                    &first_line.spans,
                    &mut spans,
                    spinner_str(0),
                    self.indicator,
                    sigil,
                    |span_idx| {
                        spinners.push((line_idx, span_idx));
                    },
                );
            }
        } else {
            let style = if matches!(output, Some(ToolOutput::Index(_))) {
                theme::current().tool_path
            } else {
                theme::current().tool
            };
            spans.extend(header_spans(tool_name, header, style, raw_input));
        }
        let mut copy = format!("{label}{gap}{header}");
        if let Some(ann) = annotation {
            spans.push(Span::styled(
                format!(" ({ann})"),
                theme::current().tool_annotation,
            ));
            write!(copy, " ({ann})").unwrap();
        }
        self.lines.push(Line::from(spans));
        self.search_text = copy;
    }

    /// The one-line form: a sigil in place of the status dot, and the inputs
    /// the header omits. The label is the same one an expanded card carries.
    fn push_compact_header(
        &mut self,
        tool_name: &str,
        header: &str,
        annotation: Option<&str>,
        raw_input: Option<&serde_json::Value>,
        output: Option<&ToolOutput>,
    ) {
        let row = compact_row(tool_name);
        let label = row.map_or(tool_name, |(_, entry)| entry.label(self.indicator.into()));
        self.sigil = row.map_or(COMPACT_FALLBACK_SIGIL, |(_, entry)| entry.sigil);

        let mut copy = format!("{label} {header}");
        let mut spans = vec![Span::styled(
            format!("{label} "),
            theme::current().tool_prefix,
        )];
        spans.extend(header_spans(
            tool_name,
            header,
            theme::current().tool,
            raw_input,
        ));
        if let Some(args) = compact_args(
            raw_input,
            header,
            row.map(|(tool, _)| tool),
            row.map_or(&[], |(_, entry)| entry.header_keys),
            output,
        ) {
            copy.push_str(&args);
            spans.push(Span::styled(args, theme::current().tool_dim));
        }
        if let Some(ann) = annotation {
            spans.push(Span::styled(
                format!(" ({ann})"),
                theme::current().tool_annotation,
            ));
            write!(copy, " ({ann})").unwrap();
        }
        self.lines.push(Line::from(spans));
        self.search_text = copy;
    }

    /// The leading glyph is the tool, coloured by outcome. A compact row has
    /// no room for a spinner beside it, so while the call runs the frame takes
    /// the slot outright.
    fn prepend_compact_sigil(&mut self, started_at: Instant) {
        if self.lines.is_empty() {
            return;
        }
        let (text, style) = match self.indicator {
            Indicator::InProgress => (
                format!("{} ", spinner_frame(started_at.elapsed().as_millis())),
                theme::current().spinner,
            ),
            finished => (format!("{} ", self.sigil), finished_style(finished)),
        };
        if matches!(self.indicator, Indicator::InProgress) {
            self.spinner_lines.push((0, 0));
        }
        self.lines[0].spans.insert(0, Span::styled(text, style));
    }

    fn push_search_text(&mut self, text: &str) {
        if !self.search_text.is_empty() {
            self.search_text.push('\n');
        }
        self.search_text.push_str(text);
    }

    /// The leading glyph is the tool, so a glance at the head says what the
    /// card is doing rather than only how it ended. The sigil holds one column
    /// in both states: the spinner takes the slot in front of it while the
    /// call runs and leaves it blank once it lands, so a finished card does
    /// not drag its header two columns left.
    fn prepend_indicator(&mut self, started_at: Instant) {
        if self.lines.is_empty() {
            return;
        }
        let theme = theme::current();
        let running = matches!(self.indicator, Indicator::InProgress);
        let (head, sigil_style) = match running {
            true => (
                Span::styled(
                    format!("{} ", spinner_frame(started_at.elapsed().as_millis())),
                    theme.spinner,
                ),
                theme.tool_prefix,
            ),
            false => (Span::raw(INDICATOR_PAD), finished_style(self.indicator)),
        };
        for (line, span) in &mut self.spinner_lines {
            if *line == 0 {
                *span += 2;
            }
        }
        if running {
            self.spinner_lines.push((0, 0));
        }
        self.lines[0].spans.splice(
            0..0,
            [head, Span::styled(format!("{} ", self.sigil), sigil_style)],
        );
    }

    /// The columns a body has once the card's own indent is taken off, which
    /// is what anything drawn under the header has to break itself to.
    fn body_width(&self) -> u16 {
        self.width.saturating_sub(TOOL_BODY_INDENT_WIDTH)
    }

    fn is_in_progress(&self) -> bool {
        matches!(self.indicator, Indicator::InProgress)
    }

    /// A running call reports what it is doing and how far it has got; a
    /// finished one is described by its output, so only the tally survives.
    fn progress_spans(&self, progress: &ToolProgress, out: &mut Vec<Span<'static>>) {
        let theme = theme::current();
        if self.is_in_progress() {
            if let Some(sigil) = activity_sigil(&progress.report.activity) {
                out.push(Span::styled(format!("{sigil} "), theme.tool_prefix));
            }
            out.push(Span::styled(
                activity_label(&progress.report.activity),
                theme.tool_prefix,
            ));
            if let Some(detail) = activity_detail(&progress.report.activity) {
                out.push(Span::styled(format!(" {detail}"), theme.tool_dim));
            }
            out.push(Span::styled(ACTIVITY_SEPARATOR, theme.tool_dim));
        }
        out.push(Span::styled(
            SubagentProgress::tally(progress.report.tools, progress.elapsed()),
            theme.tool_dim,
        ));
    }

    /// Must run after `prepend_indicator`, which owns row 0 and shifts the
    /// spinner spans sitting on it.
    fn push_progress(&mut self, progress: &ToolProgress) {
        let mut spans = vec![Span::styled(ACTIVITY_PREFIX, theme::current().tool_dim)];
        self.progress_spans(progress, &mut spans);
        self.lines.push(Line::from(spans));
        // The roster hangs off the activity row, which is itself the last node
        // under the header, so every level below it is gap rather than trunk.
        if !self.is_in_progress() {
            return;
        }
        let children = progress.report.activity.children();
        for (index, child) in children.iter().enumerate() {
            let connector = match index + 1 == children.len() {
                true => TREE_LAST,
                false => TREE_BRANCH,
            };
            self.lines.push(Line::from(activity_child_spans(
                child,
                format!("{TOOL_BODY_INDENT}{TREE_GAP}{connector}"),
            )));
        }
    }

    /// A compact row is one line by contract, so progress joins the header
    /// instead of sitting under it.
    fn append_progress(&mut self, progress: &ToolProgress) {
        if self.lines.is_empty() {
            return;
        }
        let mut spans = vec![Span::styled(ACTIVITY_SEPARATOR, theme::current().tool_dim)];
        self.progress_spans(progress, &mut spans);
        self.lines[0].spans.append(&mut spans);
    }

    fn push_code_content(&mut self, input: Option<&ToolInput>, output: Option<&ToolOutput>) {
        // `content_range` stays empty for a rendered document, because the
        // highlighting worker answers a `WriteCode` range with the file's
        // source and would splice it back over what was drawn.
        match output {
            Some(ToolOutput::WriteCode { path, lines, .. }) if renders_as_markdown(path) => {
                self.push_markdown_body(&lines.join("\n"));
            }
            _ => self.push_rendered_code(input, output),
        }
        if let Some(ToolInput::Code { code, .. } | ToolInput::Script { code, .. }) = input {
            self.push_search_text(code.trim_end());
        }
        if let Some(text) = output.and_then(|o| o.structured_display_text()) {
            self.push_search_text(&text);
        }
    }

    fn push_rendered_code(&mut self, input: Option<&ToolInput>, output: Option<&ToolOutput>) {
        let content = code_view::render_tool_content(input, output, false, self.limits.clone());
        self.truncation |= content.truncation;
        let start = self.lines.len();
        match content.source {
            Some(source) => self.source.record(start, source.indented()),
            None => self.source.abandon(),
        }
        for mut line in content.lines {
            line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
            self.lines.push(line);
        }
        self.content_range = (start, self.lines.len());
        self.rows.resize(start, None);
        self.rows.extend(content.rows);
        self.scroll_spans.extend(
            content
                .scroll_spans
                .into_iter()
                .map(|span| span.shift_lines(start)),
        );
    }

    /// The window this card's own body is drawn in. A child's comes back from
    /// the renderer, which is the only thing that knows where a child's rows
    /// landed.
    fn push_card_scroll_span(&mut self, first: usize, above: usize, below: usize) {
        let lines = self.lines.len() - first;
        self.scroll_spans.push(ScrollSpan {
            child: None,
            first,
            lines,
            total: above + lines + below,
            offset: above,
        });
    }

    /// Takes the place `push_code_content` would fill, because the call it
    /// belongs to has no output yet and its arguments are the only record of
    /// what it is about to do.
    ///
    /// `path` is what the header has said so far, which is all a still-arriving
    /// write has said about itself, and it cannot say whether that path already
    /// exists. So a document is drawn the way a *created* file settles, and an
    /// overwrite changes at settle time into the diff of what it replaced.
    /// What does not change is the card's window: a write 800 lines long does
    /// not push the transcript down 800 rows on its way past.
    fn push_live_body(&mut self, body: &str, path: &str) {
        self.source.abandon();
        let start = self.lines.len();
        let (windowed, scrolled) = match self.limits.scroll {
            Some(window) => {
                let (kept, above, below) = window_lines(body, window);
                (Cow::Owned(kept), Some((above, below)))
            }
            None => (Cow::Borrowed(body), None),
        };
        if renders_as_markdown(path) {
            self.push_markdown_body(&windowed);
        } else {
            for mut line in code_view::render_live_body(&windowed, self.body_width()) {
                line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
                self.lines.push(line);
            }
        }
        self.content_range = (start, self.lines.len());
        if let Some((above, below)) = scrolled {
            self.push_card_scroll_span(start, above, below);
            self.push_scroll_footer(above, below);
        }
    }

    /// The script a still-streaming call is spelling out, drawn the way the
    /// settled card draws it: every line, numbered from one.
    ///
    /// Deliberately not `push_live_body`. A shell or python call is drawn as a
    /// scroller, so a window on the tail would clip the command to the height
    /// its *output* is given and then let it jump to its full length the
    /// moment the call starts. The script is bounded by what the model wrote,
    /// and `render_tool_content` draws all of it for that reason.
    fn push_live_script(&mut self, code: &str) {
        self.source.abandon();
        let start = self.lines.len();
        for mut line in code_view::render_live_body(code, self.body_width()) {
            line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
            self.lines.push(line);
        }
        self.content_range = (start, self.lines.len());
        self.push_search_text(code.trim_end());
    }

    fn push_resolved_output(&mut self, resolved: &ResolvedOutput<'_>) {
        if resolved.text.is_none() {
            return;
        }

        if self.content_range.1 > self.content_range.0 {
            self.lines.push(Line::from(Span::styled(
                CODE_OUTPUT_DIVIDER,
                theme::current().tool_dim,
            )));
        }

        if let Some(text) = &resolved.text {
            let body_start = self.lines.len();
            if self.markdown {
                self.push_markdown_body(text);
            } else {
                // Broken to the body's own width, so a row too long for the
                // card keeps the indent that says whose body it is instead of
                // restarting at column zero when the terminal breaks it.
                let (body, source) = code_view::plain_body(text, self.body_width());
                self.lines.extend(indented(body, TOOL_BODY_INDENT));
                // The window has already taken its slice, so the ranges index
                // what was drawn rather than the line numbers it came from.
                self.source.record(body_start, source.indented());
            }
            if let Some(full) = &resolved.full_text {
                self.push_search_text(full);
            } else {
                self.push_search_text(text);
            }
            match resolved.scrolled {
                Some((above, below)) => {
                    self.push_card_scroll_span(body_start, above, below);
                    self.push_scroll_footer(above, below);
                }
                None => self.push_truncation_count(resolved.skipped),
            }
        }
    }

    /// Where a window sits and, while the call is still running, whether it is
    /// chasing the tail. A window with nothing either side of it says nothing:
    /// the body fits, and a footer would only claim otherwise.
    fn push_scroll_footer(&mut self, above: usize, below: usize) {
        let tail = match self.is_in_progress() {
            true => ScrollTail::Resumable,
            false => ScrollTail::Settled,
        };
        let Some(text) = scroll_footer_text(above, below, tail) else {
            return;
        };
        self.truncation = true;
        self.scroll_footer_line = Some(self.lines.len());
        let mut line = Line::from(Span::styled(text, theme::current().tool_dim));
        line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
        self.lines.push(line);
    }

    /// Discloses what the body is not showing: the reductions that ran, and the
    /// redraw frames rendering absorbed. Rendering is decoding rather than
    /// filtering, so a command can collapse redraws without being filtered at
    /// all, and then there is nothing to toggle to.
    fn push_shell_footer(&mut self, output: &ShellOutput, shell_raw: bool) {
        let redraws = output.redraws_collapsed();
        let mut parts: Vec<String> = Vec::new();
        if let Some(filter) = &output.filter {
            if shell_raw {
                parts.push("raw output".into());
            } else {
                let saved = filter
                    .unfiltered_utf8_bytes
                    .saturating_sub(filter.filtered_utf8_bytes);
                let reduction = saved
                    .saturating_mul(100)
                    .checked_div(filter.unfiltered_utf8_bytes)
                    .unwrap_or(0);
                parts.push(format!("filtered · {}", filter.stages.join(", ")));
                parts.push(format!("{reduction}% smaller"));
            }
        }
        if redraws > 0 {
            parts.push(format!("{redraws} redraws collapsed"));
        }
        if parts.is_empty() {
            return;
        }
        // Only a filtered result has a second view, and the hit test finds the
        // row by its affordance, so a line without one must not claim it.
        if output.filter.is_some() {
            let affordance = if shell_raw {
                FILTERED_AFFORDANCE
            } else {
                RAW_AFFORDANCE
            };
            parts.push(affordance.to_owned());
            self.shell_toggle_line = Some(self.lines.len());
        }
        self.lines.push(Line::from(Span::styled(
            format!("  {}", parts.join(" · ")),
            theme::current().tool_dim,
        )));
    }

    /// The markdown renderer keeps its own provenance against the text it
    /// parsed, which is not the card's source, so a card that draws prose
    /// copies by scraping rather than by slicing the wrong string.
    fn push_markdown_body(&mut self, text: &str) {
        self.source.abandon();
        let style = theme::current().assistant;
        let (painted, _) = text_to_painted(
            text,
            "",
            style,
            style,
            self.width.saturating_sub(TOOL_BODY_INDENT_WIDTH),
            Some(caudra_markdown::render::TOOL_OUTPUT_MAX_LINE_BYTES),
            Vec::new(),
        );
        for (mut line, mut links) in painted.lines.into_iter().zip(painted.links.rows) {
            line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
            links.insert(0, None);
            self.link_rows.push((self.lines.len(), links));
            self.lines.push(line);
        }
    }

    fn push_truncation_count(&mut self, skipped: usize) {
        if should_truncate(skipped) {
            self.truncation = true;
            let text = truncation_notice(skipped);
            let mut line = Line::from(Span::styled(text, theme::current().tool_dim));
            line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
            self.lines.push(line);
        }
    }

    fn push_snapshot(
        &mut self,
        snapshot: &BufferSnapshot,
        search_fallback: Option<&str>,
        started_at: Instant,
    ) {
        // A snapshot is spans a plugin painted, not a slice of anything the
        // card holds, so there is nothing for copy to index into.
        self.source.abandon();
        let base = self.lines.len();
        self.snapshot_base = Some(base);
        let total = snapshot.lines.len();
        // The window is applied to the rendered rows rather than to the text,
        // because a snapshot is already laid out and re-wrapping it here would
        // disagree with the rows a click resolves against.
        let (start, end) = match self.limits.scroll {
            Some(window) => window.range(total),
            None => (0, total),
        };
        self.snapshot_skip = start;
        let frame = spinner_str(started_at.elapsed().as_millis());
        let (lines, spinners) = snapshot_to_lines_range(
            snapshot,
            TOOL_BODY_INDENT,
            start..end,
            frame,
            self.indicator,
            self.sigil,
        );
        self.lines.extend(lines);
        self.spinner_lines
            .extend(spinners.into_iter().map(|(line, span)| (base + line, span)));
        self.push_search_text(&snapshot.text());
        if let Some(text) = search_fallback {
            self.push_search_text(text);
        }
        if self.limits.scroll.is_some() {
            self.push_scroll_footer(start, total - end);
        }
    }

    fn finish(
        self,
        input: Option<Arc<ToolInput>>,
        output: Option<Arc<ToolOutput>>,
        content_indent: &'static str,
    ) -> ToolLines {
        let highlight = HighlightRequest::new(self.content_range, input, output, self.limits);
        let mut rows = self.rows;
        rows.resize(self.lines.len(), None);
        let mut links = LinkMap::none_for(&self.lines);
        for (line, row) in self.link_rows {
            links.rows[line] = row;
        }
        let source = self
            .source
            .finish(&self.lines)
            .filter(BodySource::names_source);
        ToolLines {
            lines: self.lines,
            links,
            search_text: self.search_text,
            highlight,
            spinner_lines: self.spinner_lines,
            snapshot_base: self.snapshot_base,
            snapshot_skip: self.snapshot_skip,
            shell_toggle_line: self.shell_toggle_line,
            scroll_footer_line: self.scroll_footer_line,
            scroll_spans: self.scroll_spans,
            content_indent,
            truncation: self.truncation,
            rows,
            source,
        }
    }
}

fn indented(lines: Vec<Line<'static>>, indent: &'static str) -> Vec<Line<'static>> {
    let style = theme::current().tool;
    lines
        .into_iter()
        .map(|mut line| {
            line.spans.insert(0, Span::styled(indent, style));
            line
        })
        .collect()
}

/// Bakes snapshot spans onto `out`. `"spinner"`-styled spans bake to the
/// current frame while a tool is in progress, and `on_spinner` gets their
/// span index in the same pass, so animation offsets can never drift from
/// the baked spans. Finished tools bake the card's own sigil instead and
/// record no spinner position, so a stale `"spinner"` span can never keep
/// animating.
fn bake_spans(
    src: &[SnapshotSpan],
    out: &mut Vec<Span<'static>>,
    spinner_frame: &'static str,
    indicator: Indicator,
    sigil: char,
    mut on_spinner: impl FnMut(usize),
) {
    for span in src {
        if matches!(&span.style, SpanStyle::Named(n) if n == SPINNER_STYLE_NAME) {
            match indicator {
                Indicator::InProgress => {
                    on_spinner(out.len());
                    out.push(Span::styled(spinner_frame, theme::current().spinner));
                }
                finished => out.push(Span::styled(format!("{sigil} "), finished_style(finished))),
            }
        } else {
            out.push(Span::styled(
                span.text.clone(),
                resolve_span_style(&span.style),
            ));
        }
    }
}

fn finished_style(indicator: Indicator) -> Style {
    let theme = theme::current();
    match indicator {
        Indicator::Error => theme.tool_error,
        Indicator::Warning => theme.tool_warning,
        _ => theme.tool_success,
    }
}

fn snapshot_to_lines_range(
    snapshot: &BufferSnapshot,
    indent: &str,
    range: std::ops::Range<usize>,
    spinner_frame: &'static str,
    indicator: Indicator,
    sigil: char,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let mut spinners = Vec::new();
    let lines = snapshot.lines[range]
        .iter()
        .enumerate()
        .map(|(i, sline)| {
            let mut spans = vec![Span::raw(indent.to_string())];
            bake_spans(
                &sline.spans,
                &mut spans,
                spinner_frame,
                indicator,
                sigil,
                |span_idx| {
                    spinners.push((i, span_idx));
                },
            );
            Line::from(spans)
        })
        .collect();
    (lines, spinners)
}

pub(crate) fn resolve_span_style(style: &SpanStyle) -> Style {
    match style {
        SpanStyle::Default => theme::current().tool,
        SpanStyle::Named(name) => theme::style_by_name(name),
        SpanStyle::Inline(inline) => {
            let mut s = Style::default();
            if let Some((r, g, b)) = inline.fg {
                s = s.fg(Color::Rgb(r, g, b));
            }
            if let Some((r, g, b)) = inline.bg {
                s = s.bg(Color::Rgb(r, g, b));
            }
            if inline.bold {
                s = s.bold();
            }
            if inline.italic {
                s = s.italic();
            }
            if inline.underline {
                s = s.underlined();
            }
            if inline.dim {
                s = s.dim();
            }
            if inline.strikethrough {
                s = s.crossed_out();
            }
            if inline.reversed {
                s = s.reversed();
            }
            s
        }
    }
}

/// Whether the card's body will print this header again. A shell call's header
/// is its script's first line, so an open card carries the command twice, and
/// the body's copy is the better one: numbered, highlighted, and whole.
///
/// Compared rather than assumed from the tool name, because the same builder
/// draws a write, whose header is a path the body never repeats.
fn header_repeats_script(header: &str, input: Option<&ToolInput>) -> bool {
    input.is_some_and(|input| {
        let (ToolInput::Script { code, .. } | ToolInput::Code { code, .. }) = input;
        code.lines().next() == Some(header)
    })
}

/// Whether this tool's streaming body is a script, which an open card draws
/// whole rather than through the window its output is given.
pub(super) fn draws_live_script(tool_name: &str) -> bool {
    LIVE_SCRIPT_TOOLS
        .iter()
        .any(|known| names_tool(known, tool_name))
}

/// `expansion` is `None` on a compact row the reader has not opened, which is
/// the only state that draws a header with no body.
pub fn build_tool_lines(
    msg: &DisplayMessage,
    status: ToolStatus,
    rctx: &RenderCtx,
    expansion: Option<Disclosure>,
) -> ToolLines {
    let tool_name = msg.role.tool_name().unwrap_or("?");
    let (header, body) = match msg.text.split_once('\n') {
        Some((h, b)) => (h, Some(b)),
        None => (msg.text.as_str(), None),
    };
    let expanded = expansion.unwrap_or_default();
    // The card's own record of what is running, until a snapshot supersedes
    // it. Resolved before the header, which defers to it.
    let live = msg
        .live_body
        .as_deref()
        .filter(|_| msg.render_snapshot.is_none());

    let mut b = ToolLineBuilder::new(
        rctx.width,
        Indicator::resolve(status, msg.tool_output.as_deref()),
        rctx.limits_for(
            msg.role.tool_id(),
            expanded.full,
            rctx.resting_budget(tool_name, msg.tool_output.as_deref()),
        ),
    );
    b.apply_output_format(msg.tool_output.as_deref());
    if rctx.compact {
        b.push_compact_header(
            tool_name,
            header,
            msg.annotation.as_deref(),
            msg.tool_raw_input.as_deref(),
            msg.tool_output.as_deref(),
        );
        b.prepend_compact_sigil(rctx.started_at);
    } else {
        // The command still belongs on a row that has no body to defer to.
        // An open card has one either way: the settled script, or as much of
        // it as has streamed.
        let defers = expansion.is_some()
            && (header_repeats_script(header, msg.tool_input.as_deref())
                || (live.is_some() && draws_live_script(tool_name)));
        let shown = match defers {
            true => "",
            false => header,
        };
        b.push_header(
            tool_name,
            shown,
            msg.annotation.as_deref(),
            msg.render_header.as_ref(),
            msg.tool_output.as_deref(),
            msg.tool_raw_input.as_deref(),
        );
        b.prepend_indicator(rctx.started_at);
    }
    if let Some(progress) = msg.progress.as_ref() {
        if rctx.compact {
            b.append_progress(progress);
        } else {
            b.push_progress(progress);
        }
    }
    if expansion.is_none() {
        // Nothing is drawn below the header, but the reader still needs a
        // click target whenever there is something to reveal.
        b.truncation = msg.render_snapshot.is_some()
            || msg.tool_input.is_some()
            || msg.tool_output.is_some()
            || msg.live_body.is_some()
            || body.is_some_and(|body| !body.trim().is_empty());
        return b.finish(
            msg.tool_input.clone(),
            msg.tool_output.clone(),
            TOOL_BODY_INDENT,
        );
    }
    match live {
        Some(live) if draws_live_script(tool_name) => b.push_live_script(live),
        Some(live) => b.push_live_body(live, header),
        None => b.push_code_content(
            msg.tool_input.as_deref(),
            match msg.render_snapshot.is_some() {
                true => None,
                false => msg.tool_output.as_deref(),
            },
        ),
    }
    let show_output = if let Some(ref snapshot) = msg.render_snapshot {
        let search_text = msg
            .tool_output
            .as_ref()
            .and_then(|o| match o.as_ref() {
                ToolOutput::Plain(t) | ToolOutput::Markdown(t) | ToolOutput::ReadDir(t) => {
                    Some(t.text.as_str())
                }
                _ => None,
            })
            .or(body);
        b.push_snapshot(snapshot, search_text, rctx.started_at);
        // A denial can land while the snapshot still shows only the
        // pre-permission script preview, so the error goes below it.
        // But a collapsed snapshot keeps just a window of the output,
        // so checking the full text would duplicate long outputs. The
        // last line is a reliable probe: a tail-keep view always shows
        // it, a bare script preview never does.
        matches!(status, ToolStatus::Error) && {
            let err_text = msg.tool_output.as_deref().map(|o| o.as_text());
            let tail = err_text
                .as_deref()
                .or(body)
                .map_or("", str::trim)
                .lines()
                .next_back()
                .map_or("", str::trim);
            !tail.is_empty() && !snapshot.text().contains(tail)
        }
    } else {
        true
    };
    if show_output {
        let resolved = resolve_output(
            msg.tool_output.as_deref(),
            body,
            msg.live_output.as_deref(),
            msg.truncated_lines,
            b.limits.clone(),
            expanded.shell_raw,
        );
        b.push_resolved_output(&resolved);
    }
    if let Some(ToolOutput::Shell(output)) = msg.tool_output.as_deref() {
        b.push_shell_footer(output, expanded.shell_raw);
    }
    b.finish(
        msg.tool_input.clone(),
        msg.tool_output.clone(),
        TOOL_BODY_INDENT,
    )
}

pub fn truncate_to_header(text: &mut String) {
    let end = text.find('\n').unwrap_or(text.len());
    text.truncate(end);
}

pub(crate) fn append_annotation(ann: &mut Option<String>, suffix: &str) {
    match ann {
        Some(a) => write!(a, " · {suffix}").unwrap(),
        None => *ann = Some(suffix.to_owned()),
    }
}

/// `expanded` is `None` on an unopened compact row, which lists the loaded
/// paths instead of their contents.
pub fn build_instructions_lines(
    blocks: &[InstructionBlock],
    width: u16,
    expanded: Option<bool>,
) -> ToolLines {
    if expanded.is_none() {
        return compact_instruction_lines(blocks);
    }
    let expanded = expanded.unwrap_or_default();
    let header = blocks.first().map_or("", |b| b.path.as_str());
    let annotation = if blocks.len() > 1 {
        Some(format!("+{}", blocks.len() - 1))
    } else {
        None
    };

    let mut b = ToolLineBuilder::new(
        width,
        Indicator::Success,
        RenderLimits::new(
            expanded,
            code_view::instruction_limit(expanded),
            BatchViews::default(),
            ToolOutputLines::default(),
        ),
    );
    b.push_header("load", header, annotation.as_deref(), None, None, None);
    b.prepend_indicator(Instant::now());

    let start = b.lines.len();
    b.truncation |=
        code_view::render_instructions(blocks, &mut b.lines, b.limits.budget, false, width);
    b.source.abandon();
    for line in &mut b.lines[start..] {
        line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
    }
    b.content_range = (start, b.lines.len());

    b.push_search_text(
        &blocks
            .iter()
            .map(|bl| bl.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );

    let output = Arc::new(ToolOutput::Instructions {
        blocks: blocks.to_vec(),
    });
    b.finish(None, Some(output), TOOL_BODY_INDENT)
}

fn compact_instruction_lines(blocks: &[InstructionBlock]) -> ToolLines {
    let style = theme::current().tool_dim;
    let path_style = theme::current().tool_path;
    let mut lines = Vec::with_capacity(blocks.len());
    let mut search_text = String::new();
    for block in blocks {
        lines.push(Line::from(vec![
            Span::styled(COMPACT_LOAD_PREFIX, style),
            Span::styled(block.path.clone(), path_style),
        ]));
        if !search_text.is_empty() {
            search_text.push('\n');
        }
        write!(search_text, "{COMPACT_LOAD_PREFIX}{}", block.path).unwrap();
    }
    ToolLines {
        links: LinkMap::none_for(&lines),
        lines,
        search_text,
        highlight: None,
        spinner_lines: Vec::new(),
        snapshot_base: None,
        snapshot_skip: 0,
        shell_toggle_line: None,
        scroll_footer_line: None,
        scroll_spans: Vec::new(),
        content_indent: TOOL_BODY_INDENT,
        rows: Vec::new(),
        truncation: true,
        source: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: ToolOutputLines = ToolOutputLines::DEFAULT;
    use crate::components::{DisplayRole, ToolRole};
    use crate::markdown::TRUNCATION_PREFIX;
    use caudra_agent::tools::{
        BATCH_TOOL_NAME, FILE_GREP_TOOL_NAME, FILE_READ_TOOL_NAME, SHELL_TOOL_NAME, TASK_TOOL_NAME,
        ToolEffect,
    };
    use caudra_agent::{
        GrepFileEntry, GrepMatchGroup, ShellFilterInfo, SnapshotLine, SnapshotSpan,
        SubagentActivity, TextOutput, ToolInput, ToolOutput,
    };
    use std::time::Duration;
    use test_case::test_case;

    static NO_VIEWS: std::sync::LazyLock<BatchViewMap> =
        std::sync::LazyLock::new(BatchViewMap::new);
    static NO_PROGRESS: std::sync::LazyLock<BatchProgressMap> =
        std::sync::LazyLock::new(BatchProgressMap::new);
    static NO_LIVE: std::sync::LazyLock<BatchLiveMap> = std::sync::LazyLock::new(BatchLiveMap::new);

    fn test_rctx(width: u16) -> RenderCtx<'static> {
        RenderCtx {
            started_at: Instant::now(),
            width,
            tool_output_lines: &TOL,
            // The scroll card is exercised by its own cases, which opt in;
            // every other case asks about the budget it replaces.
            policy: CardPolicy::default(),
            card_scroll: None,
            child_scroll: Arc::default(),
            compact: false,
            batch_views: &NO_VIEWS,
            batch_progress: &NO_PROGRESS,
            batch_live: &NO_LIVE,
        }
    }

    fn scroll_rctx(width: u16, height: u32, at: ScrollWindow) -> RenderCtx<'static> {
        RenderCtx {
            policy: CardPolicy {
                scroll_card_lines: height,
                ..CardPolicy::default()
            },
            card_scroll: Some(at),
            ..test_rctx(width)
        }
    }

    fn compact_rctx(width: u16) -> RenderCtx<'static> {
        RenderCtx {
            compact: true,
            ..test_rctx(width)
        }
    }

    const BODY_IDENTITY: &str = "a body line is the indent plus what the renderer was told it had, or a filled diff row wraps";

    /// A diff row is padded to the width the limits carry, and every body line
    /// is then prefixed with the indent. The two must add back up to the width
    /// the card paints into, or each filled row wraps and the diff doubles.
    #[test_case(120 ; "wide")]
    #[test_case(40 ; "narrow")]
    #[test_case(1 ; "narrower than the indent")]
    fn the_body_width_and_its_indent_account_for_the_card(width: u16) {
        let body = test_rctx(width).limits_for(None, false, 0).width;

        assert_eq!(
            body.saturating_add(TOOL_BODY_INDENT_WIDTH),
            width.max(TOOL_BODY_INDENT_WIDTH),
            "{BODY_IDENTITY}"
        );
    }

    fn exp(full: bool) -> Disclosure {
        Disclosure {
            full,
            shell_raw: false,
        }
    }

    fn code_input() -> Option<ToolInput> {
        Some(ToolInput::Code {
            language: "sh".into(),
            code: "echo hi\n".into(),
        })
    }

    fn code_output() -> Option<ToolOutput> {
        Some(ToolOutput::ReadCode {
            path: "test.rs".into(),
            start_line: 1,
            lines: vec!["fn main() {}".into()],
            total_lines: 1,
            instructions: None,
        })
    }

    fn plain_output() -> Option<ToolOutput> {
        Some(ToolOutput::Plain("ok".into()))
    }

    fn shell_output(filtered: bool) -> ToolOutput {
        shell_output_with(filtered, 0)
    }

    fn shell_output_with(filtered: bool, redraws: u64) -> ToolOutput {
        ToolOutput::Shell(ShellOutput {
            model_text: (1..=8)
                .map(|line| format!("model_{line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            relative_workdir: ".".into(),
            timeout_ms: 120_000,
            duration_ms: 10,
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            output_limit_exceeded: false,
            final_sequence: 8,
            stdout_utf8_bytes: 47,
            stderr_utf8_bytes: 0,
            stdout: (1..=8)
                .map(|line| format!("raw_{line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            stderr: String::new(),
            stdout_capture_truncated: false,
            stderr_capture_truncated: false,
            stdout_preview_truncated: false,
            stderr_preview_truncated: false,
            stdout_redraws_collapsed: redraws,
            stderr_redraws_collapsed: 0,
            filter: filtered.then(|| ShellFilterInfo {
                stages: vec!["cargo".into(), "progress".into()],
                unfiltered_utf8_bytes: 200,
                filtered_utf8_bytes: 40,
            }),
        })
    }

    fn bash_msg(
        text: &str,
        status: ToolStatus,
        input: Option<ToolInput>,
        output: Option<ToolOutput>,
    ) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status,
                name: SHELL_TOOL_NAME.into(),
            })),
            text: text.into(),
            source: None,
            tool_input: input.map(Arc::new),
            tool_raw_input: None,
            tool_output: output.map(Arc::new),
            tool_preview_pending: false,
            live_output: None,
            live_body: None,
            annotation: None,
            progress: None,
            plan_path: None,
            truncated_lines: 0,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            body_open: None,
            thinking_duration: None,
        }
    }

    #[test_case(code_input(),  code_output(),   true,  true  ; "code_input_keeps_code_output")]
    #[test_case(None,          code_output(),   true,  true  ; "code_output_only")]
    #[test_case(None,          plain_output(),  false, false ; "no_content_no_highlight")]
    fn highlight_request(
        input: Option<ToolInput>,
        output: Option<ToolOutput>,
        expect_highlight: bool,
        expect_output: bool,
    ) {
        let msg = bash_msg("header\nbody", ToolStatus::Success, input, output);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        assert_eq!(tl.highlight.is_some(), expect_highlight);
        if let Some(hl) = &tl.highlight {
            assert_eq!(hl.output.is_some(), expect_output);
        }
    }

    fn has_styled_span(spans: &[Span<'_>], text: &str, style: Style) -> bool {
        spans
            .iter()
            .any(|s| s.content.contains(text) && s.style == style)
    }

    fn lines_text(tl: &ToolLines) -> String {
        tl.lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    const MARKDOWN_PATH: &str = "notes.md";
    const SOURCE_PATH: &str = "main.rs";
    /// A heading is the cheapest thing that reads differently drawn than
    /// quoted: rendering it spends the hashes, source keeps them.
    const HEADING_SOURCE: &str = "# Title";
    const HEADING_TEXT: &str = "Title";

    fn write_msg(
        path: &str,
        live_body: Option<&str>,
        output: Option<ToolOutput>,
    ) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Success,
                name: FILE_WRITE_TOOL_NAME.into(),
            })),
            text: path.into(),
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: output.map(Arc::new),
            live_output: None,
            tool_preview_pending: false,
            live_body: live_body.map(str::to_owned),
            annotation: None,
            progress: None,
            plan_path: None,
            truncated_lines: 0,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            body_open: None,
            thinking_duration: None,
        }
    }

    fn write_output(path: &str) -> ToolOutput {
        ToolOutput::WriteCode {
            path: path.into(),
            byte_count: HEADING_SOURCE.len(),
            lines: vec![HEADING_SOURCE.into()],
        }
    }

    #[test_case("plan.md",        true  ; "md")]
    #[test_case("NOTES.MARKDOWN", true  ; "extension_case_is_not_the_answer")]
    #[test_case("doc.mdx",        true  ; "mdx")]
    #[test_case("main.rs",        false ; "source")]
    #[test_case("README",         false ; "no_extension")]
    #[test_case("a.md.rs",        false ; "markdown_only_in_the_stem")]
    fn renders_as_markdown_matches_document_extensions(path: &str, expected: bool) {
        assert_eq!(renders_as_markdown(path), expected);
    }

    /// The header is the only thing a still-arriving write has said about
    /// itself, and it is the path.
    #[test_case(MARKDOWN_PATH, false ; "a_document_is_drawn")]
    #[test_case(SOURCE_PATH,   true  ; "source_is_quoted")]
    fn a_streaming_write_draws_its_body_by_extension(path: &str, keeps_markers: bool) {
        let msg = write_msg(path, Some(HEADING_SOURCE), None);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&tl);
        assert!(text.contains(HEADING_TEXT), "{text}");
        assert_eq!(text.contains(HEADING_SOURCE), keeps_markers, "{text}");
    }

    const UNBOUNDED_MSG: &str =
        "a created file is its own result, so the card that holds it is not abridged";
    const BOUNDED_MSG: &str =
        "a diff keeps the budget its bucket sets, which is the floor it is drawn against";

    /// The exemption belongs to the body that *is* a result, not to the tool
    /// that produced it: the same tool now also settles into a diff and a
    /// patch, and neither has any claim on an unlimited card.
    #[test]
    fn only_a_written_file_escapes_the_card_budget() {
        let rctx = test_rctx(80);
        assert_eq!(
            rctx.resting_budget(FILE_WRITE_TOOL_NAME, Some(&write_output(SOURCE_PATH))),
            usize::MAX,
            "{UNBOUNDED_MSG}"
        );

        let overwrite = ToolOutput::Diff {
            path: SOURCE_PATH.into(),
            before: HEADING_SOURCE.into(),
            after: HEADING_TEXT.into(),
            summary: String::new(),
        };
        assert_eq!(
            rctx.resting_budget(FILE_WRITE_TOOL_NAME, Some(&overwrite)),
            TOL.get(FILE_WRITE_TOOL_NAME),
            "{BOUNDED_MSG}"
        );
    }

    /// The settled card draws what the streaming one drew, and a document that
    /// asked for highlighting would have the worker splice the file's source
    /// back over it.
    #[test_case(MARKDOWN_PATH, false ; "a_document_is_drawn")]
    #[test_case(SOURCE_PATH,   true  ; "source_is_quoted")]
    fn a_settled_write_draws_its_body_by_extension(path: &str, keeps_markers: bool) {
        let msg = write_msg(path, None, Some(write_output(path)));
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&tl);
        assert!(text.contains(HEADING_TEXT), "{text}");
        assert_eq!(text.contains(HEADING_SOURCE), keeps_markers, "{text}");
        assert_eq!(tl.highlight.is_some(), keeps_markers);
    }

    /// What the header could show of a multiline command: one space-joined
    /// line, which is what the reported card was stuck on until the call ran.
    const STREAMING_COMMAND: &str = "python3 - <<'PY'\nprint(1)\nprint(2)\nPY";
    const STREAMED_HEADER: &str = "python3 - <<'PY' print(1) print(2) PY";
    const LIVE_SCRIPT_MSG: &str =
        "an open card draws the command it is being told, line by line, before the call runs";
    const LIVE_ONCE_MSG: &str = "the header defers to the body it is about to draw";

    fn streaming_shell_msg(live: &str) -> DisplayMessage {
        DisplayMessage {
            live_body: Some(live.to_owned()),
            ..bash_msg(STREAMED_HEADER, ToolStatus::InProgress, None, None)
        }
    }

    /// The reported wait: an open card showed the space-joined preview and
    /// nothing else until the whole command had streamed.
    #[test]
    fn an_open_card_draws_the_command_as_it_streams() {
        let msg = streaming_shell_msg(STREAMING_COMMAND);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &test_rctx(80),
            Some(Disclosure::default()),
        );

        let text = lines_text(&tl);
        for line in STREAMING_COMMAND.lines() {
            assert!(text.contains(line), "{LIVE_SCRIPT_MSG}: {text:?}");
        }
        assert!(!text.contains(STREAMED_HEADER), "{LIVE_ONCE_MSG}: {text:?}");
    }

    /// A shell card is a scroller, so routing the command through the live
    /// *body* would clip it to the height its output gets and then let it jump
    /// to its full length the moment the call starts.
    #[test]
    fn a_streaming_command_is_drawn_whole_rather_than_windowed() {
        /// Shorter than the command, so a window would have to hide some of it.
        const WINDOW: u32 = 2;
        let window = ScrollWindow {
            height: WINDOW as usize,
            offset: 0,
            follow: true,
        };
        let msg = streaming_shell_msg(STREAMING_COMMAND);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &scroll_rctx(80, WINDOW, window),
            Some(Disclosure::default()),
        );

        let text = lines_text(&tl);
        for line in STREAMING_COMMAND.lines() {
            assert!(text.contains(line), "{LIVE_SCRIPT_MSG}: {text:?}");
        }
        assert!(!tl.truncation, "{LIVE_SCRIPT_MSG}: {text:?}");
        assert!(!text.contains(FOLLOWING), "{LIVE_SCRIPT_MSG}: {text:?}");
    }

    /// A closed row has no body to defer to, so the one-line preview is all it
    /// has and it keeps it.
    #[test]
    fn a_closed_row_still_names_a_streaming_command() {
        let msg = streaming_shell_msg(STREAMING_COMMAND);
        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &compact_rctx(80), None);

        let text = lines_text(&tl);
        assert!(text.contains(STREAMED_HEADER), "{text:?}");
        assert!(tl.truncation, "{LIVE_SCRIPT_MSG}: {text:?}");
    }

    #[test]
    fn filtered_shell_output_defaults_to_model_view_and_can_switch_to_raw() {
        let msg = bash_msg(
            "cargo test",
            ToolStatus::Success,
            None,
            Some(shell_output(true)),
        );
        let collapsed = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let filtered_expanded = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure {
                full: true,
                ..Disclosure::default()
            }),
        );
        let raw_collapsed = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure {
                shell_raw: true,
                ..Disclosure::default()
            }),
        );
        let raw_expanded = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure {
                full: true,
                shell_raw: true,
            }),
        );
        let collapsed_text = lines_text(&collapsed);

        assert!(collapsed_text.contains("model_8"));
        assert!(!collapsed_text.contains("model_1"));
        assert!(!collapsed_text.contains("raw_8"));
        assert!(
            collapsed_text.contains("filtered · cargo, progress · 80% smaller · click for raw")
        );
        assert!(collapsed.truncation);
        assert!(collapsed.shell_toggle_line.is_some());
        assert!(lines_text(&filtered_expanded).contains("model_1"));
        assert!(lines_text(&raw_collapsed).contains("raw_8"));
        assert!(!lines_text(&raw_collapsed).contains("raw_1"));
        assert!(lines_text(&raw_collapsed).contains("raw output · click for filtered"));
        assert!(lines_text(&raw_expanded).contains("raw_1"));
        assert!(!lines_text(&raw_expanded).contains("model_1"));
    }

    #[test]
    fn unfiltered_shell_output_omits_the_view_toggle() {
        let msg = bash_msg(
            "printf raw",
            ToolStatus::Success,
            None,
            Some(shell_output(false)),
        );
        let lines = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&lines);

        assert!(text.contains("raw_8"));
        assert!(!text.contains("click for"));
        assert!(!text.contains("model_8"));
        assert!(lines.shell_toggle_line.is_none());
    }

    #[test]
    fn collapsed_redraws_are_disclosed_without_a_view_to_toggle_to() {
        // Rendering is decoding, so it runs whether or not a rule matched. A
        // reader still has to be told the frames existed, but there is no
        // second view holding them and the row must not claim a click.
        let msg = bash_msg(
            "python train.py",
            ToolStatus::Success,
            None,
            Some(shell_output_with(false, 190)),
        );
        let lines = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&lines);

        assert!(text.contains("190 redraws collapsed"), "{text}");
        assert!(!text.contains("click for"), "{text}");
        assert!(lines.shell_toggle_line.is_none());
    }

    #[test]
    fn a_filtered_result_reports_its_redraws_beside_its_stages() {
        let msg = bash_msg(
            "cargo test",
            ToolStatus::Success,
            None,
            Some(shell_output_with(true, 12)),
        );
        let lines = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&lines);

        assert!(
            text.contains(
                "filtered · cargo, progress · 80% smaller · 12 redraws collapsed · click for raw"
            ),
            "{text}"
        );
        assert!(lines.shell_toggle_line.is_some());
    }

    #[test_case(ToolStatus::InProgress, None           ; "live_streaming_shows_body")]
    #[test_case(ToolStatus::Success,    plain_output() ; "done_with_plain_output_shows_body")]
    fn bash_body_visible(status: ToolStatus, output: Option<ToolOutput>) {
        let msg = bash_msg("echo hi\nline1\nline2", status, code_input(), output);
        let tl = build_tool_lines(&msg, status, &test_rctx(80), Some(Disclosure::default()));
        let text = lines_text(&tl);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
    }

    fn line_has_styled(tl: &ToolLines, text: &str, style: Style) -> bool {
        tl.lines
            .iter()
            .any(|l| has_styled_span(&l.spans, text, style))
    }

    #[test_case("header\nbody\nmore", "header" ; "multiline")]
    #[test_case("header",            "header" ; "single_line")]
    fn truncate_to_header_cases(input: &str, expected: &str) {
        let mut text = input.to_string();
        truncate_to_header(&mut text);
        assert_eq!(text, expected);
    }

    fn tool_msg() -> DisplayMessage {
        bash_msg("cmd", ToolStatus::Success, None, None)
    }

    #[test_case(80, true  ; "shown_when_width_sufficient")]
    #[test_case(10, false ; "hidden_when_too_narrow")]
    fn append_right_info_timestamp_visibility(width: u16, expect_timestamp: bool) {
        let msg = tool_msg();
        let mut tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let span_count_before = tl.lines[0].spans.len();
        append_right_info(&mut tl.lines[0], None, Some("12:34:56"), width);
        if expect_timestamp {
            let last = tl.lines[0].spans.last().unwrap();
            assert_eq!(last.style, theme::current().timestamp);
            assert!(tl.lines[0].spans.len() > span_count_before);
        } else {
            assert_eq!(tl.lines[0].spans.len(), span_count_before);
        }
    }

    #[test]
    fn annotation_rendered_on_header() {
        let mut msg = tool_msg();
        msg.annotation = Some("2m timeout".into());
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&tl);
        assert!(text.contains("(2m timeout)"));
    }

    #[test]
    fn task_output_body_visible() {
        let msg = task_msg("**bold** and `code`".into());
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&tl);
        assert!(text.contains("bold"));
        assert!(text.contains("code"));
    }

    #[test]
    fn markdown_tool_output_retains_link_targets() {
        let msg = task_msg("[docs](https://example.com)".into());
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let (row, line) = tl
            .lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.spans.iter().any(|span| span.content == "docs"))
            .expect("markdown link line");
        let column = line
            .spans
            .iter()
            .take_while(|span| span.content != "docs")
            .map(Span::width)
            .sum::<usize>() as u16;

        assert_eq!(
            tl.links
                .target_at(&tl.lines, 80, row as u16, column)
                .as_deref(),
            Some("https://example.com")
        );
    }

    fn task_msg(output: String) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Success,
                name: TASK_TOOL_NAME.into(),
            })),
            text: "Find auth".into(),
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::Markdown(output.into()))),
            tool_preview_pending: false,
            live_output: None,
            live_body: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            body_open: None,
            thinking_duration: None,
        }
    }

    fn n_lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_truncation_styled(tl: &ToolLines) {
        let last = tl.lines.last().unwrap();
        let span = last
            .spans
            .iter()
            .find(|s| s.content.contains(TRUNCATION_PREFIX));
        assert!(span.is_some(), "expected truncation prefix");
        assert_eq!(span.unwrap().style, theme::current().tool_dim);
    }

    fn task_truncation_tl(output: String) -> ToolLines {
        let msg = task_msg(output);
        build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        )
    }

    #[test]
    fn task_output_truncated_and_styled() {
        let task_max = TOL.task;
        let tl = task_truncation_tl(n_lines(200));
        let body_lines = tl.lines.len() - 1;
        assert!(
            body_lines <= task_max + 1,
            "expected at most {} body lines, got {body_lines}",
            task_max + 1,
        );
        assert_truncation_styled(&tl);
    }

    fn assert_hr_fits(tl: &ToolLines, width: u16) {
        let hr_line = tl
            .lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains('─')));
        assert!(hr_line.is_some());
        let total_width: usize = hr_line
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        assert!(
            total_width <= width as usize,
            "HR ({total_width} chars) should fit in {width} cols"
        );
    }

    #[test]
    fn task_hr_fits_within_indented_width() {
        let width: u16 = 60;
        let msg = task_msg("before\n\n---\n\nafter".into());
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(width),
            Some(Disclosure::default()),
        );
        assert_hr_fits(&tl, width);
    }

    fn index_msg(body: &str) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Success,
                name: "file_index".into(),
            })),
            text: format!("src/lib.rs\n{body}"),
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::Plain(body.to_owned().into()))),
            tool_preview_pending: false,
            live_output: None,
            live_body: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            body_open: None,
            thinking_duration: None,
        }
    }

    #[test]
    fn index_output_truncated_at_max_lines() {
        let body: String = (0..150).map(|i| format!("  line_{i}\n")).collect();
        let msg = index_msg(&body);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&tl);
        assert!(text.contains("line_0"));
        assert!(!text.contains("line_149"));
        assert!(text.contains(TRUNCATION_PREFIX));
    }

    #[test]
    fn native_index_uses_path_style_and_click_expandable_head() {
        let lines = (1..=8)
            .map(|line| caudra_agent::IndexLine {
                output_line: line,
                text: format!("fn item_{line}() [{line}]"),
                semantic: caudra_agent::IndexLineSemantic::Item,
                body: Some(format!("fn item_{line}()")),
                source_range: Some(caudra_agent::IndexSourceRange {
                    start_line: line,
                    end_line: line,
                }),
            })
            .collect();
        let mut msg = index_msg("");
        msg.text = "src/lib.rs".into();
        msg.tool_output = Some(Arc::new(ToolOutput::Index(
            caudra_agent::IndexOutput::File {
                path: "/project/src/lib.rs".into(),
                relative_path: "src/lib.rs".into(),
                language: "rust".into(),
                skeleton: String::new(),
                lines,
                source_line_count: 8,
                parse_error: false,
                truncated: false,
                instructions: None,
                state: None,
            },
        )));
        let collapsed = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let expanded = build_tool_lines(&msg, ToolStatus::Success, &test_rctx(80), Some(exp(true)));

        assert!(collapsed.truncation);
        assert!(lines_text(&collapsed).contains("item_1"));
        assert!(!lines_text(&collapsed).contains("item_8"));
        assert!(lines_text(&expanded).contains("item_8"));
        assert!(
            collapsed.lines[0].spans.iter().any(
                |span| span.content == "src/lib.rs" && span.style == theme::current().tool_path
            )
        );
    }

    /// What `snapshot_msg`'s tool heads its rows with, sigil and trailing gap.
    const INDEX_SIGIL: &str = "≡ ";

    fn snapshot_msg(snapshot: BufferSnapshot) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Success,
                name: "file_index".into(),
            })),
            text: "src/lib.rs\nplain fallback".into(),
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::Plain("plain fallback".into()))),
            tool_preview_pending: false,
            live_output: None,
            live_body: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
            snapshot_theme_gen: 0,
            body_open: None,
            thinking_duration: None,
        }
    }

    fn make_snapshot(lines: Vec<Vec<SnapshotSpan>>) -> BufferSnapshot {
        BufferSnapshot {
            lines: Arc::new(
                lines
                    .into_iter()
                    .map(|spans| SnapshotLine { spans })
                    .collect(),
            ),
        }
    }

    #[test]
    fn snapshot_search_text_derives_from_rendered_lines() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "import asyncio".into(),
            style: SpanStyle::Named("keyword".into()),
        }]]);
        let tl = build_tool_lines(
            &snapshot_msg(snapshot),
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        assert!(
            tl.search_text.contains("import asyncio"),
            "search must index the rendered snapshot body, got: {}",
            tl.search_text
        );
    }

    #[test]
    fn snapshot_base_recorded_where_snapshot_lines_start() {
        let snapshot = make_snapshot(vec![
            vec![SnapshotSpan {
                text: "child one".into(),
                style: SpanStyle::Default,
            }],
            vec![SnapshotSpan {
                text: "child two".into(),
                style: SpanStyle::Default,
            }],
        ]);
        let tl = build_tool_lines(
            &snapshot_msg(snapshot),
            ToolStatus::InProgress,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let base = tl.snapshot_base.expect("snapshot must record its base");
        let line_text = |i: usize| {
            tl.lines[i]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        assert!(line_text(base).contains("child one"));
        assert!(line_text(base + 1).contains("child two"));
    }

    #[test]
    fn snapshot_base_absent_without_snapshot() {
        let msg = DisplayMessage {
            render_snapshot: None,
            ..snapshot_msg(make_snapshot(vec![]))
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        assert_eq!(tl.snapshot_base, None);
    }

    #[test]
    fn header_spinner_span_bakes_and_shifts_with_indicator() {
        let header = make_snapshot(vec![vec![
            SnapshotSpan {
                text: "3 tools ".into(),
                style: SpanStyle::Default,
            },
            SnapshotSpan {
                text: "· ".into(),
                style: SpanStyle::Named(SPINNER_STYLE_NAME.into()),
            },
        ]]);
        let msg = DisplayMessage {
            render_header: Some(header),
            render_snapshot: None,
            ..snapshot_msg(make_snapshot(vec![]))
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        // Head spinner, sigil, label prefix and "3 tools " all sit before the
        // spinner the plugin painted into its own header.
        assert_eq!(tl.spinner_lines, vec![(0, 4), (0, 0)]);
    }

    const DENIAL_MSG: &str = "Permission denied: user rejected";

    fn error_snapshot_msg(snapshot_lines: &[&str], output: &str) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Error,
                name: "python_execution".into(),
            })),
            text: "2 lines".into(),
            tool_output: Some(Arc::new(ToolOutput::Plain(output.into()))),
            ..snapshot_msg(make_snapshot(
                snapshot_lines
                    .iter()
                    .map(|t| {
                        vec![SnapshotSpan {
                            text: (*t).into(),
                            style: SpanStyle::Default,
                        }]
                    })
                    .collect(),
            ))
        }
    }

    #[test_case(
        &["1 print('hi')"], DENIAL_MSG,
        Some("1 print('hi')"), None
        ; "denial_shown_below_script_preview")]
    #[test_case(
        &[DENIAL_MSG], DENIAL_MSG,
        None, None
        ; "denial_in_snapshot_not_duplicated")]
    #[test_case(
        &["... (15 lines) (click to expand)", "tail line", "Exit code: 2"],
        "head line\ntail line\nExit code: 2",
        None, Some("head line")
        ; "collapsed_tail_view_not_duplicated")]
    fn error_snapshot_output_renders_once(
        snapshot: &[&str],
        output: &str,
        shown: Option<&str>,
        hidden: Option<&str>,
    ) {
        let msg = error_snapshot_msg(snapshot, output);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Error,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&tl);
        let tail = output.lines().next_back().unwrap();
        assert_eq!(
            text.matches(tail).count(),
            1,
            "output tail must render exactly once: {text}"
        );
        if let Some(shown) = shown {
            assert!(text.contains(shown), "snapshot content must stay: {text}");
        }
        if let Some(hidden) = hidden {
            assert!(
                !text.contains(hidden),
                "hidden lines must stay hidden: {text}"
            );
        }
    }

    #[test]
    fn snapshot_renders_styled_spans() {
        let snapshot = make_snapshot(vec![vec![
            SnapshotSpan {
                text: "pub".into(),
                style: SpanStyle::Named("keyword".into()),
            },
            SnapshotSpan {
                text: " fn main()".into(),
                style: SpanStyle::Named("tool".into()),
            },
        ]]);
        let msg = snapshot_msg(snapshot);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let t = theme::current();
        assert!(line_has_styled(&tl, "pub", t.index_keyword));
        assert!(line_has_styled(&tl, " fn main()", t.tool));
    }

    #[test]
    fn snapshot_overrides_text_output() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "from_snapshot".into(),
            style: SpanStyle::Default,
        }]]);
        let msg = snapshot_msg(snapshot);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&tl);
        assert!(text.contains("from_snapshot"));
        assert!(!text.contains("plain fallback"));
        assert!(
            tl.search_text.contains("plain fallback"),
            "search_text should contain tool output for Ctrl+F"
        );
    }

    #[test_case(None,       None,    "bash",  false ; "none_output_none_body")]
    #[test_case(None,       Some("hello"), "bash", true ; "none_output_with_body")]
    #[test_case(
        Some(ToolOutput::Plain("world".into())), None, "bash", true
        ; "plain_no_body_uses_plain"
    )]
    #[test_case(
        Some(ToolOutput::Plain("world".into())), Some("override"), "bash", true
        ; "body_takes_priority_over_plain"
    )]
    #[test_case(
        Some(ToolOutput::Plain(String::new().into())), None, "bash", false
        ; "empty_plain_resolves_to_none"
    )]
    #[test_case(
        Some(ToolOutput::Batch { entries: vec![], text: "legacy batch text".into() }),
        None, "batch", true
        ; "legacy_batch_falls_back_to_text"
    )]
    #[test_case(
        Some(ToolOutput::ReadDir(TextOutput { text: "dir listing".into(), instructions: None, state: None, lua_provenance: None })),
        None, "read", true
        ; "readdir_uses_text_field"
    )]
    #[test_case(
        Some(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 1, lines: vec![], total_lines: 0, instructions: None }),
        None, "read", false
        ; "structured_output_resolves_to_none"
    )]
    fn resolve_output_text_presence(
        output: Option<ToolOutput>,
        body: Option<&str>,
        tool: &str,
        expect_text: bool,
    ) {
        let limits = RenderLimits::new(false, TOL.get(tool), BatchViews::default(), TOL);
        let resolved = resolve_output(output.as_ref(), body, None, 0, limits, false);
        assert_eq!(resolved.text.is_some(), expect_text);
    }

    #[test]
    fn resolve_output_pre_truncated_forwarded() {
        let limits = RenderLimits::new(false, TOL.get("bash"), BatchViews::default(), TOL);
        let resolved = resolve_output(None, Some("short"), None, 42, limits, false);
        assert_eq!(resolved.skipped, 42);
    }

    #[test]
    fn resolve_output_truncation_overrides_pre_truncated() {
        let long = n_lines(200);
        let limits = RenderLimits::new(false, TOL.get("bash"), BatchViews::default(), TOL);
        let resolved = resolve_output(None, Some(&long), None, 5, limits, false);
        assert!(resolved.skipped > 5);
    }

    fn bash_output_msg(line_count: usize, live: bool) -> DisplayMessage {
        let full_body = n_lines(line_count);
        let tr = truncate_output(&full_body, TOL.get("bash"));
        let text = if tr.kept.is_empty() {
            "header".into()
        } else {
            format!("header\n{}", tr.kept)
        };
        let truncated_lines = tr.skipped;
        let (status, tool_output, live_output) = if live {
            (ToolStatus::InProgress, None, Some(full_body))
        } else {
            (
                ToolStatus::Success,
                Some(Arc::new(ToolOutput::Plain(full_body.into()))),
                None,
            )
        };
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status,
                name: SHELL_TOOL_NAME.into(),
            })),
            text,
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output,
            live_output,
            tool_preview_pending: false,
            live_body: None,
            annotation: None,
            progress: None,
            plan_path: None,
            truncated_lines,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            body_open: None,
            thinking_duration: None,
        }
    }

    #[test]
    fn bash_expanded_live_output() {
        let msg = bash_output_msg(200, true);
        let collapsed = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &test_rctx(80),
            Some(exp(false)),
        );
        let expanded = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &test_rctx(80),
            Some(exp(true)),
        );
        let collapsed_text = lines_text(&collapsed);
        let expanded_text = lines_text(&expanded);
        assert!(collapsed.truncation);
        assert!(!expanded.truncation);
        assert!(expanded_text.contains("line 0"));
        assert!(expanded_text.contains("line 199"));
        assert!(collapsed_text.contains("line 0"));
        assert!(!collapsed_text.contains("line 199"));
    }

    const SCROLL_HEIGHT: u32 = 10;
    const SCROLL_TOTAL: usize = 200;
    const SCROLL_WINDOW_MSG: &str =
        "a scroll card shows its window and nothing else, wherever the window sits";
    const SCROLL_NOTICE_MSG: &str =
        "the window says how much is shown, so the notice offering the rest is gone";

    fn at(offset: usize, follow: bool) -> ScrollWindow {
        ScrollWindow {
            height: SCROLL_HEIGHT as usize,
            offset,
            follow,
        }
    }

    /// Following pins the window to the tail, which is what makes a command
    /// still printing readable. Pausing holds the offset the reader scrolled
    /// to, and the body arriving underneath must not drag it along.
    #[test_case(at(0, true),   190, 199 ; "following_takes_the_tail")]
    #[test_case(at(0, false),  0,   9   ; "paused_at_the_top_holds_there")]
    #[test_case(at(40, false), 40,  49  ; "paused_midway_holds_there")]
    #[test_case(at(999, false),190, 199 ; "an_offset_past_the_end_is_clamped")]
    fn a_scroll_card_draws_only_its_window(window: ScrollWindow, first: usize, last: usize) {
        let msg = bash_output_msg(SCROLL_TOTAL, true);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &scroll_rctx(80, SCROLL_HEIGHT, window),
            Some(exp(false)),
        );
        let text = lines_text(&tl);
        // The trailing space is the row boundary: without it `line 19` also
        // matches `line 190` and the window looks wider than it is.
        let shown: Vec<usize> = (0..SCROLL_TOTAL)
            .filter(|line| text.contains(&format!("line {line} ")))
            .collect();
        assert_eq!(
            shown,
            (first..=last).collect::<Vec<_>>(),
            "{SCROLL_WINDOW_MSG}: {text}"
        );
        assert!(!text.contains("click to expand"), "{SCROLL_NOTICE_MSG}");
    }

    /// The footer is the whole of what a window says about itself, so it has
    /// to name both edges and which one the reader is pinned to.
    #[test_case(at(0, true),   FOLLOWING ; "a_followed_window_says_so")]
    #[test_case(at(40, false), PAUSED    ; "a_held_window_says_so")]
    fn a_scroll_card_reports_where_its_window_sits(window: ScrollWindow, label: &str) {
        let msg = bash_output_msg(SCROLL_TOTAL, true);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &scroll_rctx(80, SCROLL_HEIGHT, window),
            Some(exp(false)),
        );
        let text = lines_text(&tl);
        assert!(text.contains(label), "{SCROLL_NOTICE_MSG}: {text}");
    }

    #[test_case(200, true,  false, false ; "expanded_shows_all")]
    #[test_case(200, false, true,  true  ; "collapsed_truncates")]
    #[test_case(3,   false, false, false ; "short_no_truncation")]
    fn bash_output_truncation(
        line_count: usize,
        expanded: bool,
        expect_truncation: bool,
        expect_expand_notice: bool,
    ) {
        let msg = bash_output_msg(line_count, false);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(exp(expanded)),
        );
        let text = lines_text(&tl);
        assert_eq!(tl.truncation, expect_truncation);
        assert_eq!(text.contains("click to expand"), expect_expand_notice);
    }

    fn read_output_msg(line_count: usize) -> DisplayMessage {
        read_output_msg_with(line_count, "line", None)
    }

    #[test_case(20, false, true,  true  ; "read_collapsed_truncates")]
    #[test_case(20, true,  false, false ; "read_expanded_shows_all")]
    #[test_case(3,  false, false, false ; "read_short_no_truncation")]
    fn read_output_truncation(
        line_count: usize,
        expanded: bool,
        expect_truncation: bool,
        expect_expand_notice: bool,
    ) {
        let msg = read_output_msg(line_count);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(exp(expanded)),
        );
        assert_eq!(tl.truncation, expect_truncation);
        let text = lines_text(&tl);
        assert_eq!(text.contains("click to expand"), expect_expand_notice);
    }

    fn read_msg_with_instructions(code_lines: usize, instruction_lines: usize) -> DisplayMessage {
        let inst_content: String = (0..instruction_lines)
            .map(|i| format!("inst {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        read_output_msg_with(
            code_lines,
            "code",
            Some(vec![InstructionBlock {
                path: "AGENTS.md".into(),
                content: inst_content,
            }]),
        )
    }

    fn read_output_msg_with(
        line_count: usize,
        prefix: &str,
        instructions: Option<Vec<InstructionBlock>>,
    ) -> DisplayMessage {
        let lines: Vec<String> = (0..line_count).map(|i| format!("{prefix} {i}")).collect();
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Success,
                name: FILE_READ_TOOL_NAME.into(),
            })),
            text: "read /src/main.rs".into(),
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::ReadCode {
                path: "main.rs".into(),
                start_line: 1,
                lines,
                total_lines: line_count,
                instructions,
            })),
            tool_preview_pending: false,
            live_output: None,
            live_body: None,
            annotation: None,
            progress: None,
            plan_path: None,
            truncated_lines: 0,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            body_open: None,
            thinking_duration: None,
        }
    }

    #[test_case(false, true,  false ; "collapsed_truncates_instructions")]
    #[test_case(true,  false, true  ; "expanded_shows_all_instructions")]
    fn instructions_segment(expanded: bool, expect_truncation: bool, expect_all_visible: bool) {
        let msg = read_msg_with_instructions(3, 30);
        let output = msg.tool_output.as_deref().unwrap();
        let blocks = output.instructions().unwrap();
        let tl = build_instructions_lines(blocks, 80, Some(expanded));
        assert_eq!(tl.truncation, expect_truncation);
        let text = lines_text(&tl);
        assert_eq!(text.contains("inst 29"), expect_all_visible);
    }

    #[test]
    fn read_code_tool_lines_exclude_instructions() {
        let msg = read_msg_with_instructions(3, 30);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&tl);
        assert!(
            !text.contains("inst 0"),
            "instruction content should not appear in read tool lines"
        );
    }

    #[test]
    fn instructions_has_highlight_request() {
        let blocks = vec![InstructionBlock {
            path: "agents.md".into(),
            content: "follow style guide".into(),
        }];
        let tl = build_instructions_lines(&blocks, 80, Some(false));
        assert!(tl.highlight.is_some());
        let text = lines_text(&tl);
        assert!(text.contains("follow style guide"));
    }

    #[test]
    fn a_snapshot_tool_stays_one_line_until_a_compact_row_is_opened() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "rendered by lua".into(),
            style: SpanStyle::Default,
        }]]);
        let msg = snapshot_msg(snapshot);

        let collapsed = build_tool_lines(&msg, ToolStatus::Success, &compact_rctx(80), None);
        assert_eq!(collapsed.lines.len(), 1);
        assert!(
            collapsed.truncation,
            "a hidden snapshot has to leave a click target behind"
        );
        assert!(!lines_text(&collapsed).contains("rendered by lua"));

        let opened = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &compact_rctx(80),
            Some(Disclosure::default()),
        );
        assert!(lines_text(&opened).contains("rendered by lua"));
    }

    #[test]
    fn a_bodyless_compact_row_reports_nothing_to_open() {
        let msg = bash_msg("ls", ToolStatus::Success, None, None);
        let tl = build_tool_lines(&msg, ToolStatus::Success, &compact_rctx(80), None);

        assert_eq!(tl.lines.len(), 1);
        assert!(!tl.truncation);
    }

    const SUBAGENT_ELAPSED: Duration = Duration::from_millis(63_400);

    fn report(activity: SubagentActivity, tools: u32) -> SubagentProgress {
        SubagentProgress {
            activity,
            tools,
            elapsed: SUBAGENT_ELAPSED,
        }
    }

    fn running_tool_report(tools: u32) -> SubagentProgress {
        report(
            SubagentActivity::tool(Arc::from(SHELL_TOOL_NAME), "cargo nextest run"),
            tools,
        )
    }

    /// Settled progress reports the run it measured, so the row is stable to
    /// assert on and the clock cannot drift mid-test.
    fn subagent_msg(status: ToolStatus, report: Option<SubagentProgress>) -> DisplayMessage {
        let mut msg = bash_msg("find the auth middleware", status, None, None);
        msg.progress = report.map(|report| {
            let mut progress = ToolProgress::live(report);
            progress.report.elapsed = Duration::ZERO;
            progress.settle();
            progress.report.elapsed = SUBAGENT_ELAPSED;
            progress
        });
        msg
    }

    /// A collapsed row hides the body but not the progress: it is the only
    /// sign that the subagent behind it is alive.
    #[test_case(None                        ; "collapsed")]
    #[test_case(Some(Disclosure::default()) ; "expanded")]
    fn a_running_subagent_reports_its_tool_under_the_header(expansion: Option<Disclosure>) {
        let msg = subagent_msg(ToolStatus::InProgress, Some(running_tool_report(3)));

        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &test_rctx(80), expansion);

        assert_eq!(tl.lines.len(), 2, "{}", lines_text(&tl));
        let progress_line: String = tl.lines[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(
            progress_line,
            "  └── $ Running cargo nextest run · 3 tools · 1m 3.4s"
        );
    }

    const NESTED_ROSTER_MSG: &str =
        "a subagent batching draws the roster it is working through, one node in";

    fn batch_child(tool: &str, summary: &str, status: BatchToolStatus) -> ActivityChild {
        ActivityChild {
            tool: Arc::from(tool),
            summary: summary.to_owned(),
            status,
        }
    }

    /// The reported bug: a subagent running a batch reported only `Batching 3
    /// tools`, so the three calls it was actually making were invisible from
    /// the parent. They hang off the activity row as its own tree level.
    #[test]
    fn a_batching_subagent_draws_its_roster_under_the_activity() {
        let activity = SubagentActivity::batch(
            Arc::from(BATCH_TOOL_NAME),
            "3 tools",
            vec![
                batch_child(FILE_READ_TOOL_NAME, "a.rs", BatchToolStatus::Success),
                batch_child(SHELL_TOOL_NAME, "cargo check", BatchToolStatus::Running),
                batch_child(FILE_GREP_TOOL_NAME, "fn main", BatchToolStatus::Pending),
            ],
        );
        let msg = subagent_msg(ToolStatus::InProgress, Some(report(activity, 3)));

        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &test_rctx(80), None);

        let rows: Vec<String> = tl.lines.iter().skip(1).map(line_text).collect();
        assert_eq!(
            rows,
            [
                "  └── ⇶ Batching 3 tools · 3 tools · 1m 3.4s",
                "      ├── → Read a.rs",
                "      ├── $ Running cargo check",
                "      └── ⌕ Grep fn main",
            ],
            "{NESTED_ROSTER_MSG}"
        );
    }

    /// The roster describes what the call is doing, so it goes when the call
    /// stops, exactly as the activity beside it does.
    #[test]
    fn a_settled_subagent_drops_the_roster_with_its_activity() {
        let activity = SubagentActivity::batch(
            Arc::from(BATCH_TOOL_NAME),
            "1 tool",
            vec![batch_child(
                FILE_READ_TOOL_NAME,
                "a.rs",
                BatchToolStatus::Success,
            )],
        );
        let msg = subagent_msg(ToolStatus::Success, Some(report(activity, 1)));

        let tl = build_tool_lines(&msg, ToolStatus::Success, &test_rctx(80), None);

        let text = lines_text(&tl);
        assert!(!text.contains("a.rs"), "{text}");
    }

    /// A phase is not a call, so nothing names a tool on that row and the
    /// unknown-tool sigil must not stand in for one.
    #[test]
    fn a_phase_activity_draws_no_sigil() {
        let msg = subagent_msg(
            ToolStatus::InProgress,
            Some(report(SubagentActivity::Responding, 2)),
        );

        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &test_rctx(80), None);

        assert_eq!(
            line_text(&tl.lines[1]),
            "  └── Responding · 2 tools · 1m 3.4s"
        );
    }

    /// What it was doing is stale the moment it stops; what it did is not.
    #[test_case(ToolStatus::Success ; "success")]
    #[test_case(ToolStatus::Error   ; "error")]
    fn a_settled_subagent_keeps_only_its_tally(status: ToolStatus) {
        let msg = subagent_msg(status, Some(running_tool_report(7)));

        let tl = build_tool_lines(&msg, status, &test_rctx(80), Some(Disclosure::default()));

        let text = lines_text(&tl);
        assert!(text.contains("└── 7 tools · 1m 3.4s"), "{text}");
        assert!(!text.contains("cargo nextest run"), "{text}");
    }

    #[test_case(0, "1m 3.4s"          ; "nothing_run_yet_reports_only_the_clock")]
    #[test_case(1, "1 tool · 1m 3.4s" ; "one_tool_is_singular")]
    #[test_case(2, "2 tools · 1m 3.4s"; "more_than_one_is_plural")]
    fn the_tally_counts_what_the_subagent_started(tools: u32, expected: &str) {
        let msg = subagent_msg(
            ToolStatus::Success,
            Some(report(SubagentActivity::Thinking { title: None }, tools)),
        );

        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );

        assert!(
            lines_text(&tl).contains(&format!("└── {expected}")),
            "{}",
            lines_text(&tl)
        );
    }

    #[test]
    fn a_plain_tool_reports_no_progress() {
        let msg = subagent_msg(ToolStatus::InProgress, None);

        let tl = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &test_rctx(80),
            Some(Disclosure::default()),
        );

        assert!(!lines_text(&tl).contains('├'));
    }

    /// A compact row is one line by contract, so progress has to ride the
    /// header rather than claim a second row.
    #[test]
    fn a_compact_row_keeps_the_progress_on_the_header() {
        let msg = subagent_msg(ToolStatus::InProgress, Some(running_tool_report(3)));

        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &compact_rctx(80), None);

        assert_eq!(tl.lines.len(), 1);
        let text = lines_text(&tl);
        assert!(
            text.contains(" · $ Running cargo nextest run · 3 tools · 1m 3.4s"),
            "{text}"
        );
    }

    const SIGIL_WIDTH_MSG: &str =
        "a sigil holds one cell, or every label behind it sits a column out";
    const SIGIL_CLASH_MSG: &str = "two tools sharing a sigil is a family, and a family is declared";
    /// The sigils a family deliberately shares: the code graph is one tool with
    /// five verbs, the stores are one store with two, and read and write each
    /// keep the cold members whose operation really is the same.
    const SHARED_SIGILS: &[char] = &['◇', '▤', '→', '←'];

    #[test]
    fn every_sigil_occupies_one_cell() {
        for (tool, entry) in COMPACT_TOOLS {
            let width = UnicodeWidthStr::width(entry.sigil.to_string().as_str());
            assert_eq!(width, 1, "{SIGIL_WIDTH_MSG}: {tool} uses {:?}", entry.sigil);
        }
        let fallback = UnicodeWidthStr::width(COMPACT_FALLBACK_SIGIL.to_string().as_str());
        assert_eq!(fallback, 1, "{SIGIL_WIDTH_MSG}: the unknown-tool sigil");
    }

    /// The table used to hand `task` and `batch` one glyph and `shell` and
    /// `python_execution` another, so the two busiest pairs in a transcript
    /// were the two a reader could not tell apart.
    #[test]
    fn no_two_tools_share_a_sigil_outside_a_declared_family() {
        for (index, (tool, entry)) in COMPACT_TOOLS.iter().enumerate() {
            if SHARED_SIGILS.contains(&entry.sigil) {
                continue;
            }
            for (other, other_entry) in &COMPACT_TOOLS[index + 1..] {
                assert_ne!(
                    entry.sigil, other_entry.sigil,
                    "{SIGIL_CLASH_MSG}: {tool} and {other}"
                );
            }
            assert_ne!(
                entry.sigil, COMPACT_FALLBACK_SIGIL,
                "{SIGIL_CLASH_MSG}: {tool} reads as a tool the table has never heard of"
            );
        }
    }

    const ACTIVITY_VERB_MSG: &str = "an activity row names a tool by the verb its card header uses";
    const UNTABLED_TOOL: &str = "mcp_Some_unknown";
    const UNTABLED_MSG: &str = "a tool the table has never heard of answers with itself";
    const PHASE_CASE_MSG: &str = "a phase heads its row like the tool verbs beside it";
    const CONTROL_SUMMARY: &str = "lib.rs\u{1b}[2J";
    const DETAIL_ESCAPE_MSG: &str =
        "a summary is built from tool input, so the row must not pass its controls on";

    #[test_case("shell", "Running" ; "shell")]
    #[test_case("file_read", "Reading" ; "read")]
    #[test_case("file_grep", "Grepping" ; "grep")]
    #[test_case("task", "Delegating" ; "task")]
    fn an_activity_names_its_tool_by_verb(tool: &str, expected: &str) {
        let activity = SubagentActivity::tool(Arc::from(tool), "");
        assert_eq!(activity_label(&activity), expected, "{ACTIVITY_VERB_MSG}");
    }

    #[test]
    fn an_untabled_tool_answers_with_its_own_name() {
        let activity = SubagentActivity::tool(Arc::from(UNTABLED_TOOL), "");
        assert_eq!(activity_label(&activity), UNTABLED_TOOL, "{UNTABLED_MSG}");
    }

    #[test]
    fn a_phase_is_capitalised_like_the_verbs_beside_it() {
        assert_eq!(
            activity_label(&SubagentActivity::Responding),
            "Responding",
            "{PHASE_CASE_MSG}"
        );
    }

    #[test]
    fn an_activity_detail_carries_no_control_characters() {
        let activity = SubagentActivity::tool(Arc::from(SHELL_TOOL_NAME), CONTROL_SUMMARY);
        let detail = activity_detail(&activity).expect("a summary was given");
        assert!(
            !detail.contains('\u{1b}'),
            "{DETAIL_ESCAPE_MSG}: {detail:?}"
        );
    }

    #[test]
    fn compact_instructions_list_paths_instead_of_contents() {
        let blocks = vec![InstructionBlock {
            path: "site/docs/AGENTS.md".into(),
            content: "never hand-edit generated docs".into(),
        }];

        let tl = build_instructions_lines(&blocks, 80, None);

        assert_eq!(tl.lines.len(), 1);
        let text = lines_text(&tl);
        assert!(text.contains("↳ Loaded site/docs/AGENTS.md"), "{text}");
        assert!(!text.contains("never hand-edit"), "{text}");
    }

    #[test_case("filePath", "file_path" ; "camel_matches_snake")]
    #[test_case("patch_text", "patchText" ; "snake_matches_camel")]
    #[test_case("path", "path" ; "identical_keys_match")]
    fn header_keys_match_across_spellings(shown: &str, sent: &str) {
        assert!(same_key(shown, sent));
    }

    #[test]
    fn a_distinct_key_is_not_folded_into_the_header() {
        assert!(!same_key("path", "file_path"));
    }

    #[test]
    fn snapshot_empty_has_no_content_lines() {
        let snapshot = make_snapshot(vec![]);
        let msg = snapshot_msg(snapshot);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        assert!(
            !lines_text(&tl).contains("plain fallback"),
            "snapshot present means text path should not be used"
        );
        assert_eq!(tl.lines.len(), 1, "only the header line");
    }

    #[test]
    fn snapshot_within_limit_no_truncation() {
        let lines: Vec<Vec<SnapshotSpan>> = (0..3)
            .map(|i| {
                vec![SnapshotSpan {
                    text: format!("row_{i}"),
                    style: SpanStyle::Default,
                }]
            })
            .collect();
        let snapshot = make_snapshot(lines);
        let msg = snapshot_msg(snapshot);
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        let text = lines_text(&tl);
        assert!(text.contains("row_0"));
        assert!(text.contains("row_2"));
        assert!(!text.contains(TRUNCATION_PREFIX));
    }

    #[test]
    fn snapshot_search_text_uses_tool_output_not_body() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "visible".into(),
            style: SpanStyle::Default,
        }]]);
        let msg = DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Success,
                name: "file_index".into(),
            })),
            text: "src/lib.rs\nbody_text_here".into(),
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::Plain("llm_output_here".into()))),
            tool_preview_pending: false,
            live_output: None,
            live_body: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
            snapshot_theme_gen: 0,
            body_open: None,
            thinking_duration: None,
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        assert!(
            tl.search_text.contains("llm_output_here"),
            "search_text should come from ToolOutput::Plain, not body text"
        );
    }

    #[test]
    fn snapshot_search_text_falls_back_to_body_when_no_plain_output() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "visible".into(),
            style: SpanStyle::Default,
        }]]);
        let msg = DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Success,
                name: "file_index".into(),
            })),
            text: "header\nbody_fallback".into(),
            tool_preview_pending: false,
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: None,
            live_output: None,
            live_body: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
            snapshot_theme_gen: 0,
            body_open: None,
            thinking_duration: None,
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        assert!(
            tl.search_text.contains("body_fallback"),
            "search_text should fall back to msg body when no plain output"
        );
    }

    #[test]
    fn resolve_span_style_inline_all_modifiers() {
        use caudra_agent::types::InlineStyle;
        let style = SpanStyle::Inline(InlineStyle {
            fg: Some((10, 20, 30)),
            bg: Some((40, 50, 60)),
            bold: true,
            italic: true,
            underline: true,
            dim: true,
            strikethrough: true,
            reversed: true,
        });
        let resolved = resolve_span_style(&style);
        assert_eq!(resolved.fg, Some(Color::Rgb(10, 20, 30)));
        assert_eq!(resolved.bg, Some(Color::Rgb(40, 50, 60)));
        use ratatui::style::Modifier;
        assert!(resolved.add_modifier.contains(Modifier::BOLD));
        assert!(resolved.add_modifier.contains(Modifier::ITALIC));
        assert!(resolved.add_modifier.contains(Modifier::UNDERLINED));
        assert!(resolved.add_modifier.contains(Modifier::DIM));
        assert!(resolved.add_modifier.contains(Modifier::CROSSED_OUT));
        assert!(resolved.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn default_span_resolves_to_theme_tool() {
        theme::set(theme::load_by_name("dracula").expect("dracula theme"));
        assert_eq!(
            resolve_span_style(&SpanStyle::Default),
            theme::current().tool
        );
    }

    #[test]
    fn snapshot_to_lines_range_adds_indent_prefix() {
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: "content".into(),
            style: SpanStyle::Default,
        }]]);
        let (lines, _) = snapshot_to_lines_range(
            &snapshot,
            ">>",
            0..1,
            "⠋ ",
            Indicator::InProgress,
            COMPACT_FALLBACK_SIGIL,
        );
        assert_eq!(lines.len(), 1);
        let first_span = &lines[0].spans[0];
        assert_eq!(first_span.content.as_ref(), ">>");
    }

    #[test]
    fn snapshot_multi_span_line_preserves_order() {
        let snapshot = make_snapshot(vec![vec![
            SnapshotSpan {
                text: "aaa".into(),
                style: SpanStyle::Default,
            },
            SnapshotSpan {
                text: "bbb".into(),
                style: SpanStyle::Named("dim".into()),
            },
            SnapshotSpan {
                text: "ccc".into(),
                style: SpanStyle::Default,
            },
        ]]);
        let (lines, _) = snapshot_to_lines_range(
            &snapshot,
            "",
            0..1,
            "⠋ ",
            Indicator::InProgress,
            COMPACT_FALLBACK_SIGIL,
        );
        let texts: Vec<&str> = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["", "aaa", "bbb", "ccc"]);
    }

    #[test]
    fn spinner_spans_bake_to_frame_and_record_positions() {
        let snapshot = make_snapshot(vec![
            vec![SnapshotSpan {
                text: "plain".into(),
                style: SpanStyle::Default,
            }],
            vec![
                SnapshotSpan {
                    text: "before ".into(),
                    style: SpanStyle::Default,
                },
                SnapshotSpan {
                    text: "· ".into(),
                    style: SpanStyle::Named(SPINNER_STYLE_NAME.into()),
                },
            ],
        ]);
        let (lines, spinners) = snapshot_to_lines_range(
            &snapshot,
            "",
            0..2,
            "⠹ ",
            Indicator::InProgress,
            COMPACT_FALLBACK_SIGIL,
        );
        assert_eq!(spinners, vec![(1, 2)]);
        assert_eq!(lines[1].spans[2].content.as_ref(), "⠹ ");
    }

    #[test_case(ToolStatus::Success ; "success_bakes_sigil")]
    #[test_case(ToolStatus::Error ; "error_bakes_sigil")]
    fn done_snapshot_with_spinner_span_bakes_sigil_and_records_no_spinner(status: ToolStatus) {
        let snapshot = make_snapshot(vec![vec![
            SnapshotSpan {
                text: "child ".into(),
                style: SpanStyle::Default,
            },
            SnapshotSpan {
                text: "· ".into(),
                style: SpanStyle::Named(SPINNER_STYLE_NAME.into()),
            },
        ]]);
        let tl = build_tool_lines(
            &snapshot_msg(snapshot),
            status,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        assert_eq!(tl.spinner_lines, vec![]);
        let body = tl.lines.get(1).expect("snapshot body line");
        let baked = body.spans.last().expect("baked sigil span");
        assert_eq!(baked.content.as_ref(), INDEX_SIGIL);
    }

    #[test]
    fn done_header_with_spinner_span_bakes_sigil_and_records_no_spinner() {
        let header = make_snapshot(vec![vec![
            SnapshotSpan {
                text: "3 tools ".into(),
                style: SpanStyle::Default,
            },
            SnapshotSpan {
                text: "· ".into(),
                style: SpanStyle::Named(SPINNER_STYLE_NAME.into()),
            },
        ]]);
        let msg = DisplayMessage {
            render_header: Some(header),
            render_snapshot: None,
            ..snapshot_msg(make_snapshot(vec![]))
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(Disclosure::default()),
        );
        assert_eq!(tl.spinner_lines, vec![]);
        let header_line = tl.lines.first().expect("header line");
        let baked = header_line.spans.last().expect("baked sigil span");
        assert_eq!(baked.content.as_ref(), INDEX_SIGIL);
    }

    const GREP_TOOL: &str = "file_grep";
    const READ_TOOL: &str = "file_read";
    const QUERY_MARK_MSG: &str =
        "the text searched for must be marked off from the sentence built around it";

    fn header_parts(
        tool: &str,
        header: &str,
        raw_input: Option<serde_json::Value>,
    ) -> Vec<(bool, String)> {
        header_spans(tool, header, Style::default(), raw_input.as_ref())
            .iter()
            .map(|span| {
                (
                    span.style.add_modifier.contains(Modifier::ITALIC),
                    span.content.to_string(),
                )
            })
            .collect()
    }

    /// `pattern in path` arrives as one string, and a pattern may hold spaces
    /// or the very word the header joins it with, so cutting the header text
    /// would cut in the wrong place. The input is what still knows where the
    /// query ends.
    #[test_case(
        "pub mod in caudra-agent/src", "pub mod",
        &[(true, "pub mod"), (false, " in caudra-agent/src")]
        ; "the query is marked off from its root"
    )]
    #[test_case(
        "mod in src in caudra-ui", "mod in src",
        &[(true, "mod in src"), (false, " in caudra-ui")]
        ; "a pattern holding the joining word splits at its own end"
    )]
    #[test_case(
        "fn main", "fn main",
        &[(true, "fn main")]
        ; "a rootless search is all query and gains no empty tail"
    )]
    fn a_search_header_marks_the_text_it_searched_for(
        header: &str,
        pattern: &str,
        expected: &[(bool, &str)],
    ) {
        let parts = header_parts(
            GREP_TOOL,
            header,
            Some(serde_json::json!({ "pattern": pattern })),
        );
        let expected: Vec<(bool, String)> = expected
            .iter()
            .map(|(italic, text)| (*italic, (*text).to_owned()))
            .collect();

        assert_eq!(parts, expected, "{QUERY_MARK_MSG}");
    }

    /// Marking a run of the header only says something when it is known to be
    /// the query, so everything else is left as the one span it was.
    #[test_case(READ_TOOL, "src/main.rs", Some(serde_json::json!({ "file_path": "src/main.rs" })) ; "a path is not a query")]
    #[test_case(GREP_TOOL, "needle in src", None ; "a session that kept no input")]
    #[test_case(GREP_TOOL, "needle in src", Some(serde_json::json!({ "pattern": "other" })) ; "a header that does not open with the pattern")]
    #[test_case(GREP_TOOL, "needle in src", Some(serde_json::json!({ "pattern": "" })) ; "an empty pattern marks nothing")]
    fn a_header_with_no_query_to_mark_stays_one_span(
        tool: &str,
        header: &str,
        raw_input: Option<serde_json::Value>,
    ) {
        assert_eq!(
            header_parts(tool, header, raw_input),
            vec![(false, header.to_owned())],
            "{QUERY_MARK_MSG}"
        );
    }

    const QUALIFIED_MSG: &str =
        "a tool wrapped by an MCP server is the same call and must draw the same row";
    const READ_PATH: &str = "/home/u/run.rs";

    /// The reported bug: wrapping Caudra's tools in an MCP server renamed them,
    /// the exact-match lookup missed, and every row repeated its whole header
    /// back as `[filePath=…]` under a `⚙` sigil.
    #[test_case("file_read" ; "the tabled name")]
    #[test_case("mcp_File_read" ; "a server prefix and a capital")]
    #[test_case("srv.file_read" ; "a dotted qualifier")]
    #[test_case("filePath.fileRead" ; "camel case on both sides")]
    fn a_qualified_tool_resolves_to_its_own_row(tool: &str) {
        let entry = compact_tool(tool).expect(QUALIFIED_MSG);
        assert_eq!(entry.label(Tense::Past), "Read", "{QUALIFIED_MSG}");
        assert_eq!(entry.sigil, '→', "{QUALIFIED_MSG}");
    }

    /// Dropping whole segments is what keeps the qualifier honest: a bare
    /// suffix match would let any name ending in the right letters through.
    #[test_case("myfile_read" ; "a longer word ending in the tabled name")]
    #[test_case("read" ; "a segment of the tabled name is not the tool")]
    #[test_case("github.create_issue" ; "a foreign tool resolves to nothing")]
    fn an_unrelated_tool_keeps_its_own_name(tool: &str) {
        assert!(compact_tool(tool).is_none(), "{QUALIFIED_MSG}");
    }

    #[test]
    fn a_qualified_search_still_marks_its_query() {
        assert_eq!(
            header_parts(
                "mcp_File_grep",
                "pub mod in src",
                Some(serde_json::json!({ "pattern": "pub mod" })),
            ),
            vec![(true, "pub mod".to_owned()), (false, " in src".to_owned())],
            "{QUERY_MARK_MSG}"
        );
    }

    const ARGS_MSG: &str = "the brackets carry what the header does not already show";

    /// `serde_json`'s object is a `BTreeMap` until something in the build turns
    /// `preserve_order` on and makes it an `IndexMap`, so the brackets come out
    /// sorted under `-p caudra-ui` and in call order under `--workspace`. Which
    /// one a binary ships with is not this test's business; that the right
    /// pairs survive is.
    fn sorted_args(rendered: Option<String>) -> Option<String> {
        let body = rendered?;
        let mut pairs = body
            .trim_start_matches(" [")
            .trim_end_matches(']')
            .split(", ")
            .collect::<Vec<_>>();
        pairs.sort_unstable();
        Some(format!(" [{}]", pairs.join(", ")))
    }

    /// The header is the backstop for a tool the table has never heard of,
    /// which has no `header_keys` to fold anything away with.
    #[test_case(
        READ_TOOL, READ_PATH, serde_json::json!({ "filePath": READ_PATH, "offset": 1, "limit": 200 }),
        Some(" [limit=200, offset=1]")
        ; "a tabled tool folds the path away by key"
    )]
    #[test_case(
        "mcp_File_read", READ_PATH, serde_json::json!({ "filePath": READ_PATH, "offset": 1, "limit": 200 }),
        Some(" [limit=200, offset=1]")
        ; "a qualified tool folds it away the same"
    )]
    #[test_case(
        "srv.unknown", READ_PATH, serde_json::json!({ "filePath": READ_PATH, "offset": 1 }),
        Some(" [offset=1]")
        ; "an untabled tool falls back to the header text"
    )]
    #[test_case(
        "srv.unknown", READ_PATH, serde_json::json!({ "path": READ_PATH }),
        None
        ; "brackets holding only the header are not drawn at all"
    )]
    #[test_case(
        "mcp_File_grep", "pub mod in src", serde_json::json!({ "pattern": "pub mod", "path": "src", "include": "*.rs" }),
        Some(" [include=*.rs]")
        ; "a search keeps only the filter its header omits"
    )]
    fn the_brackets_never_repeat_the_header(
        tool: &str,
        header: &str,
        raw_input: serde_json::Value,
        expected: Option<&str>,
    ) {
        assert_eq!(
            sorted_args(compact_args_for(tool, header, Some(&raw_input), None)).as_deref(),
            expected,
            "{ARGS_MSG}"
        );
    }

    fn read_code_output() -> ToolOutput {
        ToolOutput::ReadCode {
            path: READ_PATH.into(),
            start_line: 190,
            lines: vec!["x".into(); 140],
            total_lines: 668,
            instructions: None,
        }
    }

    #[test]
    fn a_pending_read_keeps_its_requested_window() {
        let input = serde_json::json!({ "filePath": READ_PATH, "offset": 190, "limit": 140 });
        assert_eq!(
            sorted_args(compact_args_for(READ_TOOL, READ_PATH, Some(&input), None)).as_deref(),
            Some(" [limit=140, offset=190]"),
            "{ARGS_MSG}"
        );
    }

    #[test_case(READ_TOOL ; "the_tabled_name")]
    #[test_case("mcp_File_read" ; "a_qualified_name")]
    fn a_completed_read_folds_pagination_into_its_result(tool: &str) {
        let input = serde_json::json!({ "filePath": READ_PATH, "offset": 190, "limit": 140 });
        assert_eq!(
            compact_args_for(tool, READ_PATH, Some(&input), Some(&read_code_output())),
            None,
            "{ARGS_MSG}"
        );
    }

    #[test_case(READ_TOOL, ToolOutput::ReadDir("entry".into()) ; "a_directory_result")]
    #[test_case(READ_TOOL, ToolOutput::Plain("legacy".into()) ; "a_legacy_result")]
    #[test_case("srv.unknown", read_code_output() ; "an_unknown_tool")]
    #[test_case("websearch", read_code_output() ; "a_non_read_tool")]
    fn only_a_structured_file_read_subsumes_pagination(tool: &str, output: ToolOutput) {
        let input = serde_json::json!({ "offset": 190, "limit": 140 });
        assert_eq!(
            sorted_args(compact_args_for(tool, "", Some(&input), Some(&output))).as_deref(),
            Some(" [limit=140, offset=190]"),
            "{ARGS_MSG}"
        );
    }

    const BLOB_MSG: &str = "a payload argument never becomes the row";
    const NOTE_BODY: &str = "# Session picker\n\nThe picker merges **two** sources.";

    /// The reported bug: a stored note was pasted into the header as
    /// `[content=…]`, one `Line` deep, so its newlines vanished and its
    /// markdown read as run-on prose.
    #[test]
    fn a_stored_note_never_reaches_the_header() {
        assert_eq!(
            compact_args_for(
                "memory",
                "write session-picker.md",
                Some(&serde_json::json!({
                    "command": "write",
                    "path": "session-picker.md",
                    "content": NOTE_BODY,
                    "tags": ["ui"],
                })),
                None,
            ),
            None,
            "{BLOB_MSG}"
        );
    }

    /// What the memory row is spared by key, every other tool is spared by
    /// bound: a name the table has never heard of still cannot spill a body
    /// across the header.
    #[test]
    fn a_blob_under_an_unknown_key_is_flattened_and_cut() {
        let rendered = compact_args_for(
            "srv.unknown",
            "",
            Some(&serde_json::json!({ "note": NOTE_BODY.repeat(4) })),
            None,
        )
        .expect(BLOB_MSG);
        assert!(!rendered.contains('\n'), "{BLOB_MSG}");
        assert!(rendered.ends_with(&format!("{ELLIPSIS}]")), "{BLOB_MSG}");
        assert_eq!(
            rendered.chars().count(),
            // ` [note=` + the cut value + the ellipsis + `]`
            " [note=]".len() + COMPACT_ARG_MAX_CHARS + 1,
            "{BLOB_MSG}"
        );
    }

    /// The bound is a ceiling, not a toll: an ordinary short value still
    /// reaches the row whole.
    #[test]
    fn a_value_that_fits_is_left_as_it_came() {
        assert_eq!(
            compact_args_for(
                "srv.unknown",
                "",
                Some(&serde_json::json!({ "q": "a b" })),
                None,
            )
            .as_deref(),
            Some(" [q=a b]"),
            "{BLOB_MSG}"
        );
    }

    /// A number is what the brackets exist to carry, so one that happens to
    /// read as part of the header still has to survive.
    #[test]
    fn a_number_is_never_taken_for_the_header() {
        assert_eq!(
            compact_args_for(
                "srv.unknown",
                "src/v1/mod.rs",
                Some(&serde_json::json!({ "offset": 1 })),
                None,
            )
            .as_deref(),
            Some(" [offset=1]"),
            "{ARGS_MSG}"
        );
    }

    const DURATION_MSG: &str = "a duration input reads as one, in the unit its tool quotes";

    /// `timeout` is milliseconds to a subprocess and seconds to a fetch, so
    /// the same key and the same number have to come out a thousand-fold
    /// apart. A count is left alone: only the table says a number is a span
    /// of time, which is why an untabled tool cannot turn one into `2m`.
    #[test_case(
        "shell", serde_json::json!({ "timeout": 600_000 }), Some(" [timeout=10m]")
        ; "a subprocess quotes its timeout in millis"
    )]
    #[test_case(
        "python_execution", serde_json::json!({ "timeout": 5_000 }), Some(" [timeout=5s]")
        ; "so does the code worker"
    )]
    #[test_case(
        "webfetch", serde_json::json!({ "timeout": 30 }), Some(" [timeout=30s]")
        ; "a fetch quotes the same key in seconds"
    )]
    #[test_case(
        "websearch", serde_json::json!({ "timeoutSec": 60 }), Some(" [timeoutSec=1m]")
        ; "a key naming its unit still answers to its tool"
    )]
    #[test_case(
        "mcp_Shell", serde_json::json!({ "timeout": 120_000 }), Some(" [timeout=2m]")
        ; "a qualified tool reads the same unit"
    )]
    #[test_case(
        "file_read", serde_json::json!({ "limit": 200 }), Some(" [limit=200]")
        ; "a count is not a duration"
    )]
    #[test_case(
        "srv.unknown", serde_json::json!({ "timeout": 600_000 }), Some(" [timeout=600000]")
        ; "an untabled tool has no unit to quote"
    )]
    fn a_duration_input_is_shown_in_units_a_reader_holds(
        tool: &str,
        raw_input: serde_json::Value,
        expected: Option<&str>,
    ) {
        assert_eq!(
            compact_args_for(tool, "", Some(&raw_input), None).as_deref(),
            expected,
            "{DURATION_MSG}"
        );
    }

    const EMPTY_ANSWER_MSG: &str = "a search that found nothing says so in colour";

    fn grep_output(matches: usize) -> ToolOutput {
        let groups = (0..matches)
            .map(|i| GrepMatchGroup::single(i + 1, "hit"))
            .collect::<Vec<_>>();
        ToolOutput::GrepResult {
            entries: vec![GrepFileEntry {
                path: "src/lib.rs".into(),
                groups,
            }],
            capped: None,
        }
    }

    /// Nothing about `Success` says the answer was empty, and `0 matches`
    /// reads like any other count, so the colour is what has to carry it.
    #[test_case(grep_output(0), true  ; "a grep with no hits is a miss")]
    #[test_case(grep_output(2), false ; "a grep with hits is not")]
    #[test_case(
        ToolOutput::GrepResult { entries: Vec::new(), capped: None }, true
        ; "a grep that opened no file at all is a miss"
    )]
    #[test_case(
        ToolOutput::Plain(NO_FILES_FOUND.into()), true
        ; "a glob answering with the empty answer is a miss"
    )]
    #[test_case(
        ToolOutput::Plain("scan stopped early".into()), false
        ; "a scan that stopped early is not a confirmed miss"
    )]
    fn an_empty_answer_is_told_apart_from_a_full_one(output: ToolOutput, warns: bool) {
        assert_eq!(
            matches!(
                Indicator::resolve(ToolStatus::Success, Some(&output)),
                Indicator::Warning
            ),
            warns,
            "{EMPTY_ANSWER_MSG}"
        );
    }

    /// The colour says how the call went, so a failure keeps saying so even
    /// when its output would otherwise read as empty.
    #[test]
    fn a_failure_is_never_downgraded_to_an_empty_answer() {
        assert!(matches!(
            Indicator::resolve(ToolStatus::Error, Some(&grep_output(0))),
            Indicator::Error
        ));
    }

    #[test]
    fn an_empty_answer_and_a_full_one_do_not_share_a_colour() {
        assert_ne!(
            finished_style(Indicator::Warning),
            finished_style(Indicator::Success),
            "{EMPTY_ANSWER_MSG}"
        );
    }
}
