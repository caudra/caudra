use super::{DisplayMessage, ToolProgress, ToolStatus, escape_terminal_controls, task_card};

use super::code_view;
use super::status_bar::collapse_home;
use crate::animation::{spinner_frame, spinner_str};
use crate::chat::batch_child_id;
use crate::theme;
use caudra_config::{ClockFormat, ToolOutputLines};
use caudra_storage::background::JobKind;
use code_view::{
    BatchLiveMap, BatchProgressMap, BatchStartedMap, BatchViewMap, BatchViews, BodySource,
    CardPolicy, CodeRole, Disclosure, HighlightRegion, RenderLimits, RowTarget, ScrollSpan,
    ScrollWindow, SourceTrace, UNCONSTRAINED_WIDTH, WrappedRows,
};

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write;
use std::iter;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use jiff::Timestamp;
use jiff::tz::TimeZone;

use caudra_markdown::render::truncate_long_lines;

use crate::markdown::{LinkMap, expand_notice, should_truncate, text_to_painted};
use caudra_agent::{
    ActivityChild, BatchToolStatus, BufferSnapshot, CallStage, IndexOutput, InstructionBlock,
    NO_FILES_FOUND, ShellOutput, SnapshotSpan, SpanStyle, SubagentActivity, SubagentProgress,
    TaskCard, ToolInput, ToolOutput, format_live_duration, format_settled_duration,
    tools::{
        FILE_READ_TOOL_NAME, FILE_WRITE_TOOL_NAME, IMAGE_GENERATE_TOOL_NAME,
        LOCAL_DOCUMENT_WRITE_TOOL_NAME, MEMORY_TOOL_NAME, PYTHON_EXECUTION_TOOL_NAME,
        SHELL_TOOL_NAME, TASK_TOOL_NAME, humanize_duration, timeout_annotation,
    },
};
use caudra_workcell::{CURRENT_WORKDIR, effective_timeout, requested_workdir};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

const JSON_ESCAPE_CHARS: usize = 6;
const REPORT_TOOL_NAME: &str = "report_to_parent";
const REPORT_BLOCKED: &str = "Blocked";
const REPORT_TITLE_MAX_CHARS: usize = 80;

pub(crate) fn task_details(task: &TaskCard) -> String {
    let mut lines = Vec::new();
    if let Some(result) = &task.result {
        if let Some(error) = result.get("error").and_then(Value::as_str) {
            lines.push(format!("Error: {error}"));
        }
        let output = result.get("output").unwrap_or(result);
        if !output.is_null() && output.as_str() != Some("") {
            let shell = (task.kind == JobKind::Shell)
                .then(|| {
                    result
                        .get("shell")
                        .and_then(|value| serde_json::from_value::<ToolOutput>(value.clone()).ok())
                })
                .flatten();
            lines.push(match shell {
                Some(ToolOutput::Shell(shell)) if shell.filter.is_some() => shell.model_text,
                Some(ToolOutput::Shell(shell)) => shell.raw_text(),
                _ => readable_task_value(output),
            });
        }
    } else if let Some(preview) = &task.result_preview {
        lines.push(format!(
            "Result preview (truncated):\n{}",
            readable_task_preview(preview)
        ));
    }
    lines.extend(task.reports.iter().cloned());
    if task.result_truncated
        && let Some(reference) = &task.output_ref
    {
        lines.push(format!("Full outcome: tool_output {}", reference.id));
    } else if task.result_truncated || task.reports_truncated {
        lines.push(
            if task.kind == JobKind::Shell {
                "Output preview is truncated."
            } else {
                "Open task chat for complete output."
            }
            .into(),
        );
    }
    lines.join("\n\n")
}

fn readable_task_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => {
            let json = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
            format!("```json\n{json}\n```")
        }
    }
}

fn readable_task_preview(preview: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(preview) {
        return readable_task_value(value.get("output").unwrap_or(&value));
    }
    let preview = preview
        .split_once("\"output\":")
        .map_or(preview, |(_, output)| output)
        .trim_start();
    if preview.starts_with('"') {
        if let Some(Ok(text)) = serde_json::Deserializer::from_str(preview)
            .into_iter::<String>()
            .next()
        {
            return text;
        }
        let mut end = preview.len();
        for _ in 0..=JSON_ESCAPE_CHARS {
            if let Ok(text) = serde_json::from_str::<String>(&format!("{}\"", &preview[..end])) {
                return text;
            }
            if end == 0 {
                break;
            }
            end = preview.floor_char_boundary(end - 1);
        }
    }
    format!("```json\n{preview}\n```")
}

#[derive(Clone)]
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
    /// When each batch's still-running children started, by parent tool id.
    pub batch_started: &'a BatchStartedMap,
    pub task_cards: Option<&'a HashMap<String, TaskCard>>,
    /// The session's working directory, which a call's `workdir` argument is
    /// resolved against until its result says where the call ran. `None`
    /// names no directory for a call that has no result yet.
    pub cwd: Option<Arc<Path>>,
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
        let started = tool_id
            .and_then(|id| self.batch_started.get(id))
            .cloned()
            .unwrap_or_default();
        RenderLimits::new(full, budget, views, *self.tool_output_lines)
            .with_scroll(self.card_scroll)
            .with_policy(self.policy.clone(), self.child_scroll.clone())
            .with_progress(progress, live, started)
            .with_width(self.width.saturating_sub(TOOL_BODY_INDENT_WIDTH))
            .with_cwd(self.cwd.clone())
    }

    /// Whether this call is drawn as a fixed-height scroller rather than
    /// abridged to a budget. Turning the height down to zero gives every tool
    /// back the notice-and-click it had before.
    pub fn scrolls(&self, tool: &str) -> bool {
        self.policy.scrolls(tool)
    }

    /// The same context with each named window opened by the rows beside it,
    /// which is how a running card holds a height it has already drawn: the
    /// rows that fill it come out of the buffers those windows are already
    /// sitting on, so they are content the reader can read rather than space
    /// nothing draws into.
    ///
    /// A child's window counts as much as the card's. Inside a batch the rows
    /// a card loses are usually a nested report's, the card itself has no
    /// window at all, and the only buffers with anything left in them belong
    /// to the children; refusing to open one would leave the whole card
    /// shrinking around rows a child is already holding.
    pub fn with_windows_opened(&self, opened: &[(Option<usize>, usize)]) -> Self {
        let mut card_scroll = self.card_scroll;
        let mut child_scroll = (*self.child_scroll).clone();
        for &(child, extra) in opened {
            let window = match child {
                Some(index) => child_scroll.get_mut(&index),
                None => card_scroll.as_mut(),
            };
            if let Some(window) = window {
                window.height += extra;
            }
        }
        Self {
            card_scroll,
            child_scroll: Arc::new(child_scroll),
            ..self.clone()
        }
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
/// What separates the parts of a row that reports several things at once: a
/// header's tally from its spend, each part of its annotation from the next,
/// and a compact row's header from its activity.
const ACTIVITY_SEPARATOR: &str = " · ";
const ANNOTATION_OPEN: &str = " (";
const ANNOTATION_CLOSE: &str = ")";
/// Ends a workdir the header names, so the name reads as a directory.
const DIRECTORY_SUFFIX: char = '/';
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
/// A local document is addressed by kind and opaque reference, which together
/// are its header.
const DOCUMENT_KEYS: &[&str] = &["kind", "reference"];
/// The same, plus the document itself, which the card's body carries.
const DOCUMENT_WRITE_KEYS: &[&str] = &["kind", "reference", "content"];
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
/// The tools whose streaming body is a source the call carries entire rather
/// than a file it names, and whether the header is a summary of that same
/// source. All three draw the body as numbered lines whole, so it is rendered
/// here the way the settled card renders it and never through the window
/// their output is drawn in, and it does not move when the call stamps the
/// same value at dispatch.
///
/// A command's header is its own first line with the newlines spent as
/// spaces, so an open card defers it to the body about to spell it out. A
/// generation's header is the file it writes, which its prompt never repeats,
/// so the row keeps it and the prompt is drawn beneath. That is also why an
/// image prompt belongs here at all despite being prose: what makes the group
/// is that the argument *is* the call, so the reader watches one body from
/// the first token to the settled card.
const LIVE_SCRIPT_TOOLS: &[(&str, bool)] = &[
    (SHELL_TOOL_NAME, true),
    (PYTHON_EXECUTION_TOOL_NAME, true),
    (IMAGE_GENERATE_TOOL_NAME, false),
];
/// The tools whose streaming body is a document however it is named. Both
/// stores settle to rendered markdown, and one of them is named by an opaque
/// reference with no extension to read, so the tool is what says so rather
/// than the header.
///
/// A delegation is the one member whose live body is not what its settled card
/// draws: the prompt arrives while the subagent works, and the answer replaces
/// it on `ToolDone`. The two are different documents rather than one drawn
/// twice, so the swap is the card reporting progress, not the flicker the rule
/// against a mismatched live draw exists to prevent. Leave it.
const LIVE_MARKDOWN_TOOLS: &[&str] = &[
    MEMORY_TOOL_NAME,
    LOCAL_DOCUMENT_WRITE_TOOL_NAME,
    TASK_TOOL_NAME,
];
pub(super) const WRITING_PROMPT: &str = "Writing prompt";
pub(super) const WRITING_COMMAND: &str = "Writing command";
pub(super) const WRITING_SCRIPT: &str = "Writing script";
pub(super) const WRITING_BRIEF: &str = "Writing brief";
/// What a call is doing while the model still writes its arguments, for the
/// tools whose arguments take long enough to watch arrive. Every other tool
/// keeps its present verb: its arguments are over before a stage could be
/// read, and a generic "Writing" would pass for a file write.
const DRAFTING_LABELS: &[(&str, &str)] = &[
    (IMAGE_GENERATE_TOOL_NAME, WRITING_PROMPT),
    (SHELL_TOOL_NAME, WRITING_COMMAND),
    (PYTHON_EXECUTION_TOOL_NAME, WRITING_SCRIPT),
    (TASK_TOOL_NAME, WRITING_BRIEF),
];
/// What any call whose permission prompt is open is doing.
pub(super) const AWAITING_APPROVAL: &str = "Awaiting approval";
const MILLIS_PER_SECOND: u64 = 1_000;
const DURATION_SEPARATOR: &str = " · ";

/// Duration inputs and the millis one of their units is worth, so a bracket
/// reads `1m` instead of `60`. The unit belongs to the tool rather than to the
/// key, and reading it off the name alone could be wrong by a factor of a
/// thousand: `timeout` is seconds to a fetch, while a server's own tool may
/// count it in milliseconds. A key that names its own unit still gets a row,
/// because the tool it belongs to is what says the name is a duration at all.
///
/// The two command runners are absent on purpose. Their deadline is always on
/// the row, default and all, so it is annotated rather than bracketed and
/// folded away by `header_keys`.
const DURATION_ARGS: &[(&str, &str, u64)] = &[
    ("webfetch", "timeout", MILLIS_PER_SECOND),
    ("websearch", "timeoutSec", MILLIS_PER_SECOND),
];
/// What separates a server or namespace from the tool it qualifies.
const QUALIFIER: [char; 3] = ['_', '.', '-'];

/// How a tool names itself on a compact row. The `name> ` prefix is gone
/// there, so the label is what identifies the call, and `header_keys` are the
/// inputs the row already shows elsewhere -- in the header text, or in the
/// annotation a timeout or a workdir is named by -- so the `[k=v]` suffix can
/// skip them.
/// Tools missing from the table fall back to their registered name.
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
            BatchToolStatus::Drafting
            | BatchToolStatus::Pending
            | BatchToolStatus::AwaitingApproval
            | BatchToolStatus::Error => Self::Plain,
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
const REPORT: Inflection = ("Report", "Reporting", "Reported");
const VIEW: Inflection = ("View", "Viewing", "Viewed");
const DRAW: Inflection = ("Generate image", "Generating image", "Generated image");
/// A store reached by sub-command. The verb is the `command` argument, which
/// the `[k=v]` suffix already shows, so the row names the store instead.
const MAP: Inflection = ("Map", "Mapping", "Mapped");
const LOCATE: Inflection = ("Locate", "Locating", "Located");
const TRACE: Inflection = ("Trace", "Tracing", "Traced");
const IMPACT: Inflection = ("Impact", "Assessing", "Assessed");
const EXPAND: Inflection = ("Expand", "Expanding", "Expanded");
const MEMORY: Inflection = ("Memory", "Memory", "Memory");
const SESSIONS: Inflection = ("Sessions", "Sessions", "Sessions");

/// The verbs a store's header opens with. A tool reached by sub-command is the
/// one case where the label cannot carry the tense, because the verb is an
/// argument and varies per call; the header carries it instead, and the tool
/// emits the plain form once for the row to conjugate.
const MEMORY_COMMANDS: &[(&str, Inflection)] = &[
    ("list", ("list", "listing", "listed")),
    ("read", ("read", "reading", "read")),
    ("write", ("write", "writing", "wrote")),
    ("delete", ("delete", "deleting", "deleted")),
];

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
    tool_row("shell", '$', RUN, &["command", "timeoutSec", "workdir"]),
    tool_row("python_execution", 'λ', COMPUTE, &["code", "timeoutSec"]),
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
    tool_row(
        REPORT_TOOL_NAME,
        '↑',
        REPORT,
        &["title", "message", "blocked"],
    ),
    // `command` is folded by name rather than left to the containment check,
    // because the header spells the verb in a tense the argument never had.
    tool_row("memory", '▤', MEMORY, &["content", "command", "path"]),
    tool_row("sessions", '▤', SESSIONS, &[]),
    tool_row("local_document_read", '→', READ, DOCUMENT_KEYS),
    tool_row("local_document_write", '←', WRITE, DOCUMENT_WRITE_KEYS),
    tool_row("local_document_apply_patch", '±', PATCH, DOCUMENT_KEYS),
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
/// The current row stays in the present tense: the call may yet be cut off, and
/// the past tense would assert it finished. A row a later activity replaced is
/// finished by construction, so it takes the past and reads like the settled
/// rows of the batch roster drawn under it.
pub(super) fn activity_label(activity: &SubagentActivity, tense: Tense) -> String {
    let past = matches!(tense, Tense::Past);
    match activity {
        // A row replaced before its call ran is a call that never ran, so it
        // takes the plain verb the way a call that did not finish well does.
        SubagentActivity::Tool {
            name,
            stage: Some(_),
            ..
        } if past => title(name, Tense::Plain, None).1.to_owned(),
        SubagentActivity::Tool { name, stage, .. } => title(name, tense, *stage).1.to_owned(),
        phase if past => capitalized(phase.past_label()),
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
pub(super) fn activity_sigil(activity: &SubagentActivity, tense: Tense) -> Option<char> {
    match activity {
        SubagentActivity::Tool { name, .. } => Some(compact_sigil_label(name, tense).0),
        _ => None,
    }
}

/// One child of a batch a subagent is running, drawn the way the batch card
/// draws the same call: the connector, then the sigil in its outcome colour,
/// then the tense the child's status puts the verb in.
pub(super) fn activity_child_spans(child: &ActivityChild, prefix: String) -> Vec<Span<'static>> {
    let theme = theme::current();
    let (sigil, label, tense) = title(&child.tool, child.status.into(), child.status.stage());
    let mut spans = vec![
        Span::styled(prefix, theme.tool_dim),
        Span::styled(format!("{sigil} "), batch_sigil_style(child.status, None)),
        Span::styled(label.to_owned(), theme.tool_prefix),
    ];
    if !child.summary.is_empty() {
        let summary = inflected_header(&child.tool, &child.summary, tense);
        spans.push(Span::styled(
            format!(" {}", escape_terminal_controls(&summary)),
            theme.tool_dim,
        ));
    }
    spans
}

fn append_activity_spans(
    activity: &SubagentActivity,
    tense: Tense,
    spans: &mut Vec<Span<'static>>,
) {
    let theme = theme::current();
    if let Some(sigil) = activity_sigil(activity, tense) {
        spans.push(Span::styled(format!("{sigil} "), theme.tool_prefix));
    }
    spans.push(Span::styled(
        activity_label(activity, tense),
        theme.tool_prefix,
    ));
    if let Some(detail) = activity_detail(activity) {
        spans.push(Span::styled(format!(" {detail}"), theme.tool_dim));
    }
}

pub(super) fn progress_lines(
    progress: &ToolProgress,
    continuation: &str,
    width: u16,
) -> Vec<Line<'static>> {
    let theme = theme::current();
    let mut lines = Vec::new();
    let mut activities = progress.activities().peekable();
    while let Some((activity, current)) = activities.next() {
        let (connector, trunk) = if activities.peek().is_some() {
            (TREE_BRANCH, TREE_TRUNK)
        } else {
            (TREE_LAST, TREE_GAP)
        };
        let mut spans = vec![Span::styled(
            format!("{continuation}{connector}"),
            theme.tool_dim,
        )];
        let tense = match current {
            true => Tense::Present,
            false => Tense::Past,
        };
        append_activity_spans(activity, tense, &mut spans);
        lines.push(Line::from(clamp_to_row(spans, width)));
        let children = activity.children();
        for (index, child) in children.iter().enumerate() {
            let connector = if index + 1 == children.len() {
                TREE_LAST
            } else {
                TREE_BRANCH
            };
            let row = activity_child_spans(child, format!("{continuation}{trunk}{connector}"));
            lines.push(Line::from(clamp_to_row(row, width)));
        }
    }
    lines
}

/// Spans cut to a single row of `width` columns, with an ellipsis standing
/// where the rest was dropped.
///
/// A status row names what a subagent is doing at this instant and is
/// rewritten every time that changes. Letting one wrap would make its height a
/// function of how long the current command happens to be, so walking through
/// calls of different lengths would reflow every row under it. The text these
/// rows summarise is reachable by opening the call they name.
///
/// A caller with no width to give is left alone, the way every other renderer
/// here treats [`UNCONSTRAINED_WIDTH`].
pub(super) fn clamp_to_row(spans: Vec<Span<'static>>, width: u16) -> Vec<Span<'static>> {
    if width == UNCONSTRAINED_WIDTH {
        return spans;
    }
    let width = usize::from(width);
    let drawn: usize = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if drawn <= width {
        return spans;
    }

    let mut room = width.saturating_sub(UnicodeWidthChar::width(ELLIPSIS).unwrap_or(1));
    let mut kept = Vec::with_capacity(spans.len());
    for span in spans {
        let span_width = UnicodeWidthStr::width(span.content.as_ref());
        if span_width <= room {
            room -= span_width;
            kept.push(span);
            continue;
        }
        let mut cut = String::with_capacity(span.content.len());
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if grapheme_width > room {
                break;
            }
            room -= grapheme_width;
            cut.push_str(grapheme);
        }
        cut.push(ELLIPSIS);
        kept.push(Span::styled(cut, span.style));
        break;
    }
    kept
}

/// A phase label is authored lowercase to read mid-sentence, but on this row it
/// stands where a tool's inflected verb would and has to match it.
fn capitalized(label: &str) -> String {
    let mut rest = label.chars();
    rest.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(rest).collect()
    })
}

/// The header with its leading verb put in `tense`, borrowed unchanged for
/// every tool that does not open with one. Only a whole leading word is
/// replaced, so a note called `write-ups.md` keeps its name.
pub(super) fn inflected_header<'a>(tool: &str, header: &'a str, tense: Tense) -> Cow<'a, str> {
    if !names_tool(MEMORY_TOOL_NAME, tool) {
        return Cow::Borrowed(header);
    }
    let (verb, rest) = header.split_once(' ').unwrap_or((header, ""));
    let Some((_, inflection)) = MEMORY_COMMANDS.iter().find(|(name, _)| *name == verb) else {
        return Cow::Borrowed(header);
    };
    let conjugated = match tense {
        Tense::Plain => inflection.0,
        Tense::Present => inflection.1,
        Tense::Past => inflection.2,
    };
    match rest.is_empty() {
        true => Cow::Borrowed(conjugated),
        false => Cow::Owned(format!("{conjugated} {rest}")),
    }
}

pub(super) fn report_header<'a>(
    tool: &str,
    header: &'a str,
    raw_input: Option<&Value>,
) -> Cow<'a, str> {
    if !names_tool(REPORT_TOOL_NAME, tool) {
        return Cow::Borrowed(header);
    }
    let first_line = |text: &str| {
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| {
                let mut title = escape_terminal_controls(line);
                if let Some((cut, _)) = title.char_indices().nth(REPORT_TITLE_MAX_CHARS) {
                    title.truncate(cut);
                    title.push(ELLIPSIS);
                }
                title
            })
    };
    let title = raw_input
        .and_then(|input| input.get("title"))
        .and_then(Value::as_str)
        .and_then(first_line)
        .or_else(|| report_message(tool, raw_input).and_then(first_line))
        .or_else(|| {
            if names_tool(REPORT_TOOL_NAME, header.trim()) {
                None
            } else {
                first_line(header)
            }
        })
        .unwrap_or_default();
    let blocked = raw_input
        .and_then(|input| input.get("blocked"))
        .and_then(Value::as_bool);
    Cow::Owned(match blocked {
        Some(true) if title.is_empty() => REPORT_BLOCKED.to_owned(),
        Some(true) => format!("{REPORT_BLOCKED}{ACTIVITY_SEPARATOR}{title}"),
        _ => title,
    })
}

pub(super) fn report_message<'a>(tool: &str, raw_input: Option<&'a Value>) -> Option<&'a str> {
    if !names_tool(REPORT_TOOL_NAME, tool) {
        return None;
    }
    raw_input?
        .get("message")?
        .as_str()
        .filter(|message| !message.trim().is_empty())
}

pub(super) fn report_markdown(message: &str) -> String {
    let mut markdown = String::with_capacity(message.len());
    for character in message.replace("\r\n", "\n").chars() {
        if character.is_control() && !matches!(character, '\n' | '\t') {
            markdown.extend(character.escape_default());
        } else {
            markdown.push(character);
        }
    }
    markdown
}

/// How a tool introduces itself on a one-line row. A name the table has never
/// heard of answers with itself, which is all there is to say about it.
pub(super) fn compact_sigil_label(name: &str, tense: Tense) -> (char, &str) {
    compact_tool(name).map_or((COMPACT_FALLBACK_SIGIL, name), |entry| {
        (entry.sigil, entry.label(tense))
    })
}

/// The label a call's stage puts in place of its verb, `None` where the verb
/// already says as much.
fn stage_label(tool: &str, stage: CallStage) -> Option<&'static str> {
    match stage {
        CallStage::Drafting => DRAFTING_LABELS
            .iter()
            .find(|(known, _)| names_tool(known, tool))
            .map(|(_, label)| *label),
        CallStage::AwaitingApproval => Some(AWAITING_APPROVAL),
    }
}

/// How a title opens: the tool's sigil, then the stage its call is in when
/// that has a name, and its verb in `tense` otherwise, with the tense the rest
/// of the title takes. A named stage is one the call has not run past, so the
/// verb a store's header opens with reads as the request it still is.
///
/// Every surface that names a call opens it here, so a card, its compact row,
/// a batch child and a subagent's activity cannot call one call two things.
pub(super) fn title(tool: &str, tense: Tense, stage: Option<CallStage>) -> (char, &str, Tense) {
    let staged = stage.and_then(|stage| stage_label(tool, stage));
    let tense = match staged {
        Some(_) => Tense::Plain,
        None => tense,
    };
    let (sigil, verb) = compact_sigil_label(tool, tense);
    (sigil, staged.unwrap_or(verb), tense)
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
    if matches!(output, Some(ToolOutput::Tasks(_))) {
        return None;
    }
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
    pub highlight: Vec<HighlightRequest>,
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

#[derive(Clone)]
pub struct HighlightRequest {
    pub region: HighlightRegion,
    pub input: Option<Arc<ToolInput>>,
    pub output: Option<Arc<ToolOutput>>,
}

impl HighlightRequest {
    pub fn sources(&self) -> (Option<&ToolInput>, Option<&ToolOutput>) {
        let mut input = self.input.as_deref();
        let mut output = self.output.as_deref();
        for index in &self.region.path {
            let Some(ToolOutput::Batch { entries, .. }) = output else {
                return (None, None);
            };
            let Some(entry) = entries.get(*index) else {
                return (None, None);
            };
            input = entry.input.as_ref();
            output = entry.output.as_ref();
        }
        match self.region.role {
            CodeRole::Input => (input, None),
            CodeRole::Output => (None, output),
        }
    }

    pub fn matches(&self, other: &Self) -> bool {
        if !self.same_view(other) {
            return false;
        }
        let (input, output) = self.sources();
        let (other_input, other_output) = other.sources();
        input == other_input
            && match (output, other_output) {
                (None, None) => true,
                (Some(left), Some(right)) => {
                    std::ptr::eq(left, right) || same_syntax_output(left, right)
                }
                _ => false,
            }
    }

    fn same_view(&self, other: &Self) -> bool {
        self.region.path == other.region.path
            && self.region.role == other.region.role
            && self.region.limits.width == other.region.limits.width
            && self.region.limits.budget == other.region.limits.budget
            && self.region.transforms == other.region.transforms
    }

    pub fn append_compatible(&self, other: &Self) -> bool {
        if self.region.role != CodeRole::Input || !self.same_view(other) {
            return false;
        }
        match (self.sources().0, other.sources().0) {
            (
                Some(ToolInput::Code { language, code }),
                Some(ToolInput::Code {
                    language: next_language,
                    code: next_code,
                }),
            )
            | (
                Some(ToolInput::Script { language, code }),
                Some(ToolInput::Script {
                    language: next_language,
                    code: next_code,
                }),
            ) => language == next_language && next_code.starts_with(code),
            _ => false,
        }
    }

    pub fn input_source(&self) -> Option<String> {
        self.sources().0.map(|input| {
            let (ToolInput::Code { code, .. } | ToolInput::Script { code, .. }) = input;
            code.trim_end_matches('\n')
                .lines()
                .collect::<Vec<_>>()
                .join("\n")
        })
    }
}

fn same_syntax_output(left: &ToolOutput, right: &ToolOutput) -> bool {
    match (left, right) {
        (
            ToolOutput::ReadCode {
                path: lp,
                start_line: ls,
                lines: ll,
                ..
            },
            ToolOutput::ReadCode {
                path: rp,
                start_line: rs,
                lines: rl,
                ..
            },
        ) => (lp, ls, ll) == (rp, rs, rl),
        (
            ToolOutput::WriteCode {
                path: lp,
                lines: ll,
                ..
            },
            ToolOutput::WriteCode {
                path: rp,
                lines: rl,
                ..
            },
        ) => (lp, ll) == (rp, rl),
        (
            ToolOutput::Diff {
                path: lp,
                before: lb,
                after: la,
                ..
            },
            ToolOutput::Diff {
                path: rp,
                before: rb,
                after: ra,
                ..
            },
        ) => (lp, lb, la) == (rp, rb, ra),
        (ToolOutput::Patch { files: left }, ToolOutput::Patch { files: right }) => left
            .iter()
            .map(|f| (&f.path, &f.patch, f.additions, f.deletions, f.truncated))
            .eq(right
                .iter()
                .map(|f| (&f.path, &f.patch, f.additions, f.deletions, f.truncated))),
        (
            ToolOutput::GrepResult {
                entries: left,
                capped: lc,
            },
            ToolOutput::GrepResult {
                entries: right,
                capped: rc,
            },
        ) => {
            lc.as_ref().map(|c| (c.files_scanned, c.files_listed))
                == rc.as_ref().map(|c| (c.files_scanned, c.files_listed))
                && left.len() == right.len()
                && left.iter().zip(right).all(|(l, r)| {
                    l.path == r.path
                        && l.groups.len() == r.groups.len()
                        && l.groups.iter().zip(&r.groups).all(|(l, r)| {
                            l.lines
                                .iter()
                                .map(|l| (l.line_nr, &l.text, l.is_match))
                                .eq(r.lines.iter().map(|l| (l.line_nr, &l.text, l.is_match)))
                        })
                })
        }
        (
            ToolOutput::Index(IndexOutput::File {
                language: ll,
                lines: lr,
                ..
            }),
            ToolOutput::Index(IndexOutput::File {
                language: rl,
                lines: rr,
                ..
            }),
        ) => (ll, lr) == (rl, rr),
        (
            ToolOutput::CodeGraph {
                headline: lh,
                rows: lr,
                source: ls,
                footer: lf,
                ..
            },
            ToolOutput::CodeGraph {
                headline: rh,
                rows: rr,
                source: rs,
                footer: rf,
                ..
            },
        ) => (lh, lr, ls, lf) == (rh, rr, rs, rf),
        (ToolOutput::Instructions { blocks: left }, ToolOutput::Instructions { blocks: right }) => {
            left.iter()
                .map(|b| (&b.path, &b.content))
                .eq(right.iter().map(|b| (&b.path, &b.content)))
        }
        _ => false,
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
        if output.is_some_and(task_card::has_active) {
            return Self::InProgress;
        }
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
        BatchToolStatus::Drafting
        | BatchToolStatus::Pending
        | BatchToolStatus::AwaitingApproval => theme.tool_dim,
        BatchToolStatus::Running => theme.spinner,
        BatchToolStatus::Success => finished_style(Indicator::resolve(ToolStatus::Success, output)),
        BatchToolStatus::Error => theme.tool_error,
    }
}

/// How much of a body a card draws, counted in the rows it paints into rather
/// than in the source lines behind them. One source line is any number of rows
/// once it wraps, so a limit spent on lines leaves a card's height following
/// the length of whatever happens to be in it.
///
/// Applied by the painter, since only it knows how many rows a line of this
/// text takes at this width.
#[derive(Clone, Copy)]
enum RowLimit {
    /// The reader's own position in the body. The footer under it says where
    /// it sits, and nothing it leaves out is beyond reach.
    Scroll(ScrollWindow),
    /// The height the card rests at, `tail` for a body whose newest rows are
    /// the ones worth keeping. Whatever falls outside is what the notice
    /// offering the rest reports.
    Budget { height: usize, tail: bool },
}

impl RowLimit {
    /// What a body nothing abridges is drawn under.
    const WHOLE: Self = Self::Budget {
        height: usize::MAX,
        tail: false,
    };

    /// The rows kept, and how many were withheld above and below them.
    ///
    /// A budget pays for its own notice: the row saying what is missing comes
    /// out of the budget rather than on top of it, so a card that had to
    /// abridge stands exactly as tall as one that did not. That is also what
    /// keeps the count honest — one row over budget costs the body a row and
    /// so withholds two, never the single row no notice is allowed to report.
    fn apply<T>(self, rows: Vec<T>) -> (Vec<T>, Option<(usize, usize)>) {
        let window = match self {
            Self::Scroll(window) => Some(window),
            Self::Budget { height, tail } => (rows.len() > height).then(|| ScrollWindow {
                height: height.saturating_sub(1),
                offset: 0,
                follow: tail,
            }),
        };
        code_view::window_rows(rows, window)
    }
}

struct ResolvedOutput<'a> {
    text: Option<Cow<'a, str>>,
    full_text: Option<Cow<'a, str>>,
    /// Lines cut on the way in rather than by the card's own limit. It never
    /// painted them and so cannot count their rows, so the notice adds them to
    /// the rows it withheld itself.
    dropped: usize,
    limit: RowLimit,
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
    let (raw_text, dropped): (Option<Cow<'a, str>>, usize) = if whole {
        match &full_text {
            Some(t) => (Some(t.clone()), 0),
            None if output.is_some() => (None, 0),
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
            (None, None) => (None, 0),
        }
    };

    // A window is the reader's own position in the body, so it outranks both
    // the budget and the expansion a click would otherwise have granted.
    let limit = match limits.scroll.filter(|_| !expanded) {
        Some(window) => RowLimit::Scroll(window),
        None => RowLimit::Budget {
            height: limits.budget,
            // A command's newest output is the part worth keeping; every other
            // body reports what it did from the top down.
            tail: matches!(output, Some(ToolOutput::Shell(_))),
        },
    };

    let text = raw_text.filter(|text| !text.is_empty());
    ResolvedOutput {
        // Bounding how wide one source line may paint bounds the rows it can
        // charge the budget, so a body with a megabyte on a single line cannot
        // spend the whole card on it. A window has no budget to protect.
        text: match limit {
            RowLimit::Budget { .. } => text.map(capped_lines),
            RowLimit::Scroll(_) => text,
        },
        full_text,
        dropped,
        limit,
    }
}

/// `text` with no source line longer than the renderer will draw, borrowed
/// still when it already was.
fn capped_lines(text: Cow<'_, str>) -> Cow<'_, str> {
    let capped = match truncate_long_lines(&text) {
        Cow::Owned(capped) => Some(capped),
        Cow::Borrowed(_) => None,
    };
    capped.map_or(text, Cow::Owned)
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
    highlights: Vec<HighlightRegion>,
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
    /// Leading spans of row 0 that are the card's head rather than what the
    /// header says. Declared by whichever function put them there, because a
    /// spinner frame and a tool's sigil are ordinary text to look at and a
    /// wrapped header would otherwise restart underneath them.
    head: usize,
    /// Where the call is before it runs, which its title names in place of
    /// the verb its indicator would give it.
    stage: Option<CallStage>,
}

impl ToolLineBuilder {
    fn new(width: u16, indicator: Indicator, limits: RenderLimits) -> Self {
        Self {
            stage: None,
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
            highlights: Vec::new(),
            content_range: (0, 0),
            rows: Vec::new(),
            source: SourceTrace::default(),
            width,
            truncation: false,
            limits,
            markdown: false,
            indicator,
            head: 0,
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
        annotation: Vec<Span<'static>>,
        render_header: Option<&BufferSnapshot>,
        output: Option<&ToolOutput>,
        raw_input: Option<&serde_json::Value>,
    ) {
        let (sigil, label, tense) = title(tool_name, self.indicator.into(), self.stage);
        let header = &*inflected_header(tool_name, header, tense);
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
        push_annotation(&mut spans, &mut copy, annotation);
        self.lines.push(Line::from(spans));
        self.search_text = copy;
    }

    /// The one-line form: a sigil in place of the status dot, and the inputs
    /// the header omits. The label is the same one an expanded card carries.
    fn push_compact_header(
        &mut self,
        tool_name: &str,
        header: &str,
        annotation: Vec<Span<'static>>,
        raw_input: Option<&serde_json::Value>,
        output: Option<&ToolOutput>,
    ) {
        let (sigil, label, tense) = title(tool_name, self.indicator.into(), self.stage);
        let row = compact_row(tool_name);
        let header = &*inflected_header(tool_name, header, tense);
        self.sigil = sigil;

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
        push_annotation(&mut spans, &mut copy, annotation);
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
        self.head += 1;
    }

    /// Row 0 only, and appended rather than inserted: `prepend_indicator` and
    /// `prepend_compact_sigil` own the front of that row and shift the spinner
    /// spans sitting on it. Deliberately not in `search_text`, because a number
    /// that changes every frame is nothing a reader can search for.
    fn append_duration(&mut self, elapsed: Duration) {
        let Some(line) = self.lines.first_mut() else {
            return;
        };
        let clock = match self.indicator {
            Indicator::InProgress => format_live_duration(elapsed),
            _ => format_settled_duration(elapsed),
        };
        line.spans.push(Span::styled(
            format!("{DURATION_SEPARATOR}{clock}"),
            theme::current().tool_dim,
        ));
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
        self.head += 2;
    }

    /// The columns a body has once the card's own indent is taken off, which
    /// is what anything drawn under the header has to break itself to.
    fn body_width(&self) -> u16 {
        self.width.saturating_sub(TOOL_BODY_INDENT_WIDTH)
    }

    fn is_in_progress(&self) -> bool {
        matches!(self.indicator, Indicator::InProgress)
    }

    /// Must run after `prepend_indicator`, which owns row 0 and shifts the
    /// spinner spans sitting on it.
    ///
    /// A running call reports what it is doing; a finished one is described by
    /// its output, and the tally both of them answer for sits in the header, so
    /// a settled card has nothing left to draw here.
    fn push_progress(&mut self, progress: &ToolProgress) {
        if !self.is_in_progress() {
            return;
        }
        self.lines
            .extend(progress_lines(progress, TOOL_BODY_INDENT, self.width));
    }

    fn push_progress_body(
        &mut self,
        progress: &ToolProgress,
        first: usize,
        window: Option<ScrollWindow>,
    ) {
        let output_end = self.lines.len();
        self.push_progress(progress);
        self.content_range = (0, 0);
        self.source.abandon();
        let Some(window) = window else {
            return;
        };
        let (body, hidden) = code_view::window_rows(self.lines.split_off(first), Some(window));
        let (above, below) = hidden.unwrap_or_default();
        let start = first + above;
        let end = start + body.len();
        self.highlights.retain_mut(|region| {
            if region.range.end <= first {
                return true;
            }
            if !region.keep(&(start..end)) {
                return false;
            }
            region.shift(first);
            true
        });
        let keep_row = |line: &mut usize| {
            if *line < first {
                return true;
            }
            if !(start..end).contains(line) {
                return false;
            }
            *line -= above;
            true
        };
        self.link_rows.retain_mut(|(line, _)| keep_row(line));
        self.spinner_lines.retain_mut(|(line, _)| keep_row(line));
        self.shell_toggle_line = self
            .shell_toggle_line
            .filter(|line| (start..end).contains(line));
        self.shell_toggle_line = self.shell_toggle_line.map(|line| line - above);
        if let Some(base) = self.snapshot_base {
            self.snapshot_skip += start.saturating_sub(base);
            self.snapshot_base =
                (base.max(start) < output_end.min(end)).then_some(base.max(start) - above);
        }
        self.rows.resize(end, None);
        self.rows.drain(first..start);
        self.lines.extend(body);
        self.push_card_scroll_span(first, above, below);
        if let Some(span) = self.scroll_spans.last_mut() {
            span.history_start = Some(output_end - first);
        }
        self.push_scroll_footer(above, below);
    }

    /// A compact row is one line by contract, so progress joins the header
    /// instead of sitting under it.
    fn append_progress(&mut self, progress: &ToolProgress) {
        if self.lines.is_empty() || !self.is_in_progress() {
            return;
        }
        let mut spans = vec![Span::styled(ACTIVITY_SEPARATOR, theme::current().tool_dim)];
        append_activity_spans(&progress.report.activity, Tense::Present, &mut spans);
        self.lines[0].spans.append(&mut spans);
    }

    fn push_code_content(&mut self, input: Option<&ToolInput>, output: Option<&ToolOutput>) {
        match output {
            Some(ToolOutput::WriteCode { path, lines, .. }) if renders_as_markdown(path) => {
                self.push_markdown_body(&lines.join("\n"), RowLimit::WHOLE);
            }
            _ => self.push_rendered_code(input, output),
        }
        if let Some(ToolInput::Code { code, .. } | ToolInput::Script { code, .. }) = input {
            self.push_search_text(code.trim_end());
        }
        if let Some(text) = output.and_then(|o| o.structured_display_text()) {
            self.push_search_text(&text);
        }
        if let Some(ToolOutput::Batch { text, .. }) = output {
            self.push_search_text(text);
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
        for (mut line, mut links) in content.lines.into_iter().zip(content.links.rows) {
            line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
            links.insert(0, None);
            self.link_rows.push((self.lines.len(), links));
            self.lines.push(line);
        }
        self.highlights
            .extend(content.highlights.into_iter().map(|mut region| {
                region.shift(start);
                region.indent(TOOL_BODY_INDENT.into(), Style::default());
                region
            }));
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
            extent_lines: lines,
            total: above + lines + below,
            offset: above,
            history_start: None,
        });
    }

    /// Takes the place `push_code_content` would fill, because the call it
    /// belongs to has no output yet and its arguments are the only record of
    /// what it is about to do.
    ///
    /// All a still-arriving write has said about itself is its header, and a
    /// header cannot say whether the path it names already exists. So a
    /// document is drawn the way a *created* file settles, and an overwrite
    /// changes at settle time into the diff of what it replaced. What does not
    /// change is the card's window: a write 800 lines long does not push the
    /// transcript down 800 rows on its way past.
    ///
    /// `markdown` is [`draws_live_markdown`], decided by the caller because a
    /// body is drawn the way its settled card will draw it and only the caller
    /// knows which tool is writing.
    fn push_live_body(&mut self, body: &str, markdown: bool) {
        self.source.abandon();
        let start = self.lines.len();
        let limit = self.limits.scroll.map_or(RowLimit::WHOLE, RowLimit::Scroll);
        let scrolled = if markdown {
            self.push_markdown_body(body, limit)
        } else {
            let (rows, scrolled) =
                limit.apply(code_view::render_live_body(body, self.body_width()));
            for mut line in rows {
                line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
                self.lines.push(line);
            }
            scrolled
        };
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
            let scrolled = if self.markdown {
                self.push_markdown_body(text, resolved.limit)
            } else {
                // Broken to the body's own width, so a row too long for the
                // card keeps the indent that says whose body it is instead of
                // restarting at column zero when the terminal breaks it.
                let (body, source) = code_view::plain_body(text, self.body_width());
                let (body, scrolled) = resolved.limit.apply(body);
                // The rows behind the lines the limit kept, so a copy reads
                // back what was drawn rather than the whole body it came from.
                let source = match scrolled {
                    Some((above, _)) => source.keep_rows(above..above + body.len()),
                    None => Some(source),
                };
                self.lines.extend(indented(body, TOOL_BODY_INDENT));
                match source {
                    Some(source) => self.source.record(body_start, source.indented()),
                    None => self.source.abandon(),
                }
                scrolled
            };
            if let Some(full) = &resolved.full_text {
                self.push_search_text(full);
            } else {
                self.push_search_text(text);
            }
            match resolved.limit {
                RowLimit::Scroll(_) => {
                    if let Some((above, below)) = scrolled {
                        self.push_card_scroll_span(body_start, above, below);
                        self.push_scroll_footer(above, below);
                    }
                }
                // Both halves of the count come out of the same cut, so the
                // notice cannot claim a number the body did not withhold.
                RowLimit::Budget { .. } => {
                    let withheld = scrolled.map_or(0, |(above, below)| above + below);
                    self.push_truncation_count(withheld + resolved.dropped);
                }
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
    fn push_markdown_body(&mut self, text: &str, limit: RowLimit) -> Option<(usize, usize)> {
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
        // A heading, a table and a fence each break differently, so the rows
        // exist only once the renderer has run; the limit is taken on them.
        let painted: Vec<_> = painted.lines.into_iter().zip(painted.links.rows).collect();
        let (painted, scrolled) = limit.apply(painted);
        for (mut line, mut links) in painted {
            line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
            links.insert(0, None);
            self.link_rows.push((self.lines.len(), links));
            self.lines.push(line);
        }
        scrolled
    }

    /// The rows the budget held back, counted in the rows a reader would have
    /// read them in. The notice sits in the budget rather than beside it, so
    /// this is never the one row it would not be allowed to report.
    fn push_truncation_count(&mut self, withheld: usize) {
        if should_truncate(withheld) {
            self.truncation = true;
            let text = expand_notice(&format!("{withheld} rows"));
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
            self.push_card_scroll_span(base, start, total - end);
            self.push_scroll_footer(start, total - end);
        }
    }

    fn finish(
        self,
        input: Option<Arc<ToolInput>>,
        output: Option<Arc<ToolOutput>>,
        content_indent: &'static str,
    ) -> ToolLines {
        let source = self
            .source
            .finish(&self.lines)
            .filter(BodySource::names_source);
        let mut rows = self.rows;
        rows.resize(self.lines.len(), None);
        // The card is laid out in logical lines and broken here, once, so
        // everything it indexed by line moves onto the rows that break made
        // rather than being recorded against rows that no longer exist.
        let wrapped = WrappedRows::new(self.lines, self.head, self.width);
        let lines = wrapped.lines();
        let mut links = LinkMap::none_for(&lines);
        for (line, row) in self.link_rows {
            let first = wrapped.row_of(line);
            for (offset, spans) in wrapped.spans_of(line, &row).into_iter().enumerate() {
                links.rows[first + offset] = spans;
            }
        }
        ToolLines {
            lines,
            links,
            search_text: self.search_text,
            highlight: self
                .highlights
                .into_iter()
                .map(|mut region| {
                    region.wrap(&wrapped, self.width);
                    HighlightRequest {
                        region,
                        input: input.clone(),
                        output: output.clone(),
                    }
                })
                .collect(),
            spinner_lines: self
                .spinner_lines
                .into_iter()
                .map(|(line, span)| wrapped.span_at(line, span))
                .collect(),
            snapshot_base: self.snapshot_base.map(|line| wrapped.row_of(line)),
            snapshot_skip: self.snapshot_skip,
            shell_toggle_line: self.shell_toggle_line.map(|line| wrapped.row_of(line)),
            scroll_footer_line: self.scroll_footer_line.map(|line| wrapped.row_of(line)),
            scroll_spans: self
                .scroll_spans
                .into_iter()
                .map(|span| wrapped.scroll_span(span))
                .collect(),
            content_indent,
            truncation: self.truncation,
            rows: wrapped.expand(rows),
            source: source.map(|source| wrapped.body(source)),
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
/// whole rather than through the window its output is given, and whether the
/// header summarises that same script. `None` for every tool whose live body
/// is neither.
fn live_script(tool_name: &str) -> Option<bool> {
    LIVE_SCRIPT_TOOLS
        .iter()
        .find(|(known, _)| names_tool(known, tool_name))
        .map(|(_, names_header)| *names_header)
}

pub(super) fn draws_live_script(tool_name: &str) -> bool {
    live_script(tool_name).is_some()
}

/// Whether a still-arriving body is drawn as the document it is rather than as
/// its source. A tool that settles to rendered markdown says so by name, since
/// its header is a sub-command or an opaque reference and has no extension to
/// read; everything else is judged by the path the header has spelled so far.
fn draws_live_markdown(tool_name: &str, header: &str) -> bool {
    LIVE_MARKDOWN_TOOLS
        .iter()
        .any(|known| names_tool(known, tool_name))
        || renders_as_markdown(header)
}

/// A shell call's clock: wall time while it runs, and the command's own
/// measured time once it lands, which leaves out the dispatch either side of
/// it and so can step down slightly on settle.
///
/// Only `shell` reports a duration of its own, and that one is persisted with
/// the output, so it is the one tool whose card reads the same restored as it
/// did live. The running branch has to ask the tool's name; the settled one
/// does not, because nothing else produces a [`ToolOutput::Shell`].
pub(super) fn shell_elapsed(msg: &DisplayMessage, status: ToolStatus) -> Option<Duration> {
    if status == ToolStatus::InProgress {
        return msg
            .tool_started
            .filter(|_| names_tool(SHELL_TOOL_NAME, msg.role.tool_name().unwrap_or_default()))
            .map(|started| started.elapsed());
    }
    match msg.tool_output.as_deref() {
        Some(ToolOutput::Shell(output)) => Some(Duration::from_millis(output.duration_ms)),
        _ => None,
    }
}

/// What the header says in parentheses: how much work the call has done, then
/// what it spent doing it.
///
/// Composed here rather than stored, because a running subagent's clock keeps
/// counting past its last report and a stored string would show it stopped.
/// The card is rebuilt every frame for the spinner, so this is measured as
/// often as it is drawn.
fn header_annotation(msg: &DisplayMessage) -> Option<String> {
    let tally = msg
        .progress
        .as_ref()
        .map(|progress| SubagentProgress::tally(progress.report.tools, progress.elapsed()));
    match (tally, msg.annotation.clone()) {
        (Some(tally), Some(spend)) => Some(format!("{tally}{ACTIVITY_SEPARATOR}{spend}")),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

/// `expansion` is `None` on a compact row the reader has not opened, which is
/// the only state that draws a header with no body.
pub fn build_tool_lines(
    msg: &DisplayMessage,
    status: ToolStatus,
    rctx: &RenderCtx,
    expansion: Option<Disclosure>,
) -> ToolLines {
    let projected = msg
        .role
        .tool_id()
        .and_then(|call_id| {
            project_task_output(
                call_id,
                msg.tool_output.as_deref(),
                rctx.task_cards,
                msg.live_output.as_deref(),
                rctx.batch_live,
            )
        })
        .map(|output| {
            let mut projected = msg.clone();
            projected.tool_output = Some(Arc::new(output));
            projected.render_snapshot = None;
            projected.render_header = None;
            projected.live_body = None;
            truncate_to_header(&mut projected.text);
            projected
        });
    let msg = projected.as_ref().unwrap_or(msg);
    let tool_name = msg.role.tool_name().unwrap_or("?");
    let (mut header, body) = match msg.text.split_once('\n') {
        Some((h, b)) => (h, Some(b)),
        None => (msg.text.as_str(), None),
    };
    if expansion.is_some() && matches!(msg.tool_output.as_deref(), Some(ToolOutput::Tasks(_))) {
        header = "";
    }
    let report_title = report_header(tool_name, header, msg.tool_raw_input.as_deref());
    let header = report_title.as_ref();
    let is_report = names_tool(REPORT_TOOL_NAME, tool_name);
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
    b.stage = msg.tool_stage;
    b.apply_output_format(msg.tool_output.as_deref());
    let mut report = header_annotation(msg);
    if let Some(timeout) = header_timeout(tool_name, msg.tool_raw_input.as_deref()) {
        append_annotation(&mut report, &timeout_annotation(timeout));
    }
    let workdir = header_workdir(
        tool_name,
        msg.tool_raw_input.as_deref(),
        msg.tool_output.as_deref(),
        rctx.cwd.as_deref(),
    );
    let annotation = annotation_spans(report.as_deref(), workdir);
    if rctx.compact {
        b.push_compact_header(
            tool_name,
            header,
            annotation,
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
                || (live.is_some() && live_script(tool_name) == Some(true)));
        let shown = match defers {
            true => "",
            false => header,
        };
        b.push_header(
            tool_name,
            shown,
            annotation,
            msg.render_header.as_ref().filter(|_| !is_report),
            msg.tool_output.as_deref(),
            msg.tool_raw_input.as_deref(),
        );
        b.prepend_indicator(rctx.started_at);
    }
    if let Some(elapsed) = shell_elapsed(msg, status) {
        b.append_duration(elapsed);
    }
    let progress_body = msg
        .progress
        .as_ref()
        .filter(|progress| {
            (b.is_in_progress() || progress.is_live()) && (!rctx.compact || expansion.is_some())
        })
        .map(|progress| {
            let window = b
                .limits
                .scroll
                .or_else(|| b.limits.policy.window(tool_name, 0, true));
            if window.is_some() {
                b.limits.scroll = None;
                b.limits.budget = usize::MAX;
            }
            (progress, b.lines.len(), window)
        });
    if let Some(progress) = msg.progress.as_ref().filter(|_| progress_body.is_none()) {
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
            || report_message(tool_name, msg.tool_raw_input.as_deref()).is_some()
            || msg.tool_input.is_some()
            || msg.tool_output.is_some()
            || msg.live_body.is_some()
            || body.is_some_and(|body| !body.trim().is_empty());
        if let Some((progress, first, window)) = progress_body {
            b.push_progress_body(progress, first, window);
        }
        return b.finish(
            msg.tool_input.clone(),
            msg.tool_output.clone(),
            TOOL_BODY_INDENT,
        );
    }
    if is_report {
        if let Some(message) = report_message(tool_name, msg.tool_raw_input.as_deref()) {
            let start = b.lines.len();
            let (lines, source, links) =
                code_view::markdown_body(&report_markdown(message), b.body_width());
            b.lines.extend(indented(lines, TOOL_BODY_INDENT));
            b.source.record(start, source.indented());
            for (index, mut row) in links.rows.into_iter().enumerate() {
                row.insert(0, None);
                b.link_rows.push((start + index, row));
            }
            b.content_range = (start, b.lines.len());
            b.push_search_text(message);
        }
        let resolved = resolve_output(
            msg.tool_output.as_deref(),
            body,
            msg.live_output.as_deref(),
            msg.truncated_lines,
            RenderLimits {
                budget: usize::MAX,
                scroll: None,
                ..b.limits.clone()
            },
            false,
        );
        b.push_resolved_output(&resolved);
        return b.finish(None, None, TOOL_BODY_INDENT);
    }
    match live {
        Some(live) if draws_live_script(tool_name) => b.push_live_script(live),
        Some(live) => b.push_live_body(live, draws_live_markdown(tool_name, header)),
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
        let window = progress_body.and_then(|(_, _, window)| window);
        let output_limits = match window {
            Some(window) => RenderLimits {
                budget: window.height,
                scroll: Some(window),
                ..b.limits.clone()
            },
            None => b.limits.clone(),
        };
        let mut resolved = resolve_output(
            msg.tool_output.as_deref(),
            body,
            msg.live_output.as_deref(),
            msg.truncated_lines,
            output_limits,
            expanded.shell_raw,
        );
        if window.is_some() {
            resolved.limit = RowLimit::WHOLE;
            resolved.dropped = 0;
        }
        b.push_resolved_output(&resolved);
    }
    if let Some(ToolOutput::Shell(output)) = msg.tool_output.as_deref() {
        b.push_shell_footer(output, expanded.shell_raw);
    }
    if let Some((progress, first, window)) = progress_body {
        b.push_progress_body(progress, first, window);
    }
    b.finish(
        msg.tool_input.clone(),
        msg.tool_output.clone(),
        TOOL_BODY_INDENT,
    )
}

fn project_task_output(
    call_id: &str,
    output: Option<&ToolOutput>,
    cards: Option<&HashMap<String, TaskCard>>,
    live: Option<&str>,
    batch_live: &BatchLiveMap,
) -> Option<ToolOutput> {
    if let Some(ToolOutput::Tasks(tasks)) = output {
        let mut projected = None;
        for (index, task) in tasks.iter().enumerate() {
            if let Some(card) = cards.and_then(|cards| cards.get(&task.call_id))
                && card.invocation_id == task.invocation_id
                && card != task
            {
                let task = &mut projected.get_or_insert_with(|| tasks.clone())[index];
                task.state = card.state.clone();
                task.background = card.background;
                task.updated_at = card.updated_at;
            }
            let current = projected.as_ref().map_or(task, |tasks| &tasks[index]);
            let live = if task.call_id == call_id {
                live
            } else {
                batch_child_id(&task.call_id)
                    .and_then(|(parent, index)| batch_live.get(parent)?.get(&index))
                    .map(String::as_str)
            };
            if current.kind == JobKind::Shell
                && current.active()
                && let Some(live) = live
            {
                let task = &mut projected.get_or_insert_with(|| tasks.clone())[index];
                task.result = Some(serde_json::json!({"output": live}));
                task.result_preview = None;
            }
        }
        return projected.map(ToolOutput::Tasks);
    }
    if let Some(card) = cards.and_then(|cards| cards.get(call_id)) {
        if !card.background && !card.active() {
            return None;
        }
        let output = ToolOutput::Tasks(vec![card.clone()]);
        return project_task_output(call_id, Some(&output), None, live, batch_live)
            .or(Some(output));
    }
    if let Some(ToolOutput::Batch { entries, text }) = output {
        let mut projected = None;
        for (index, entry) in entries.iter().enumerate() {
            if let Some(output) = project_task_output(
                &format!("{call_id}:{index}"),
                entry.output.as_ref(),
                cards,
                batch_live
                    .get(call_id)
                    .and_then(|children| children.get(&index))
                    .map(String::as_str),
                batch_live,
            ) {
                let entries = projected.get_or_insert_with(|| entries.clone());
                entries[index].output = Some(output);
            }
        }
        return projected.map(|entries| ToolOutput::Batch {
            entries,
            text: text.clone(),
        });
    }
    None
}

pub fn truncate_to_header(text: &mut String) {
    let end = text.find('\n').unwrap_or(text.len());
    text.truncate(end);
}

/// The deadline a command runs under, for the header to name while there is
/// still something to name it about.
///
/// Read from the stored input rather than pushed from the call, so a restored
/// card says what a live one said. The row is resolved first because a call
/// wrapped by an MCP server arrives qualified, and the executor's rules are
/// keyed by the canonical name.
pub(super) fn header_timeout(
    tool: &str,
    raw_input: Option<&serde_json::Value>,
) -> Option<Duration> {
    let (tool, _) = compact_row(tool)?;
    effective_timeout(tool, raw_input?)
}

/// The directory a command starts in, for the header to name when it is not
/// the session's own.
///
/// The result says where the call really ran once there is one, which is also
/// all a restored card needs. Until then the argument is resolved the way the
/// executor will resolve it, so the words do not change when the call lands,
/// and a call that never got to run still names where it was sent.
pub(super) fn header_workdir(
    tool: &str,
    raw_input: Option<&serde_json::Value>,
    output: Option<&ToolOutput>,
    cwd: Option<&Path>,
) -> Option<String> {
    let relative = match output {
        Some(ToolOutput::Shell(output)) => Cow::Borrowed(output.relative_workdir.as_str()),
        _ => Cow::Owned(requested_workdir(compact_row(tool)?.0, raw_input?, cwd?)?),
    };
    (relative != CURRENT_WORKDIR).then(|| workdir_label(&relative))
}

/// Home folded to `~` the way the status bar folds the session's own
/// directory, and a trailing slash that says the name is a directory.
fn workdir_label(relative: &str) -> String {
    let mut label = collapse_home(relative);
    if !label.ends_with(DIRECTORY_SUFFIX) {
        label.push(DIRECTORY_SUFFIX);
    }
    label
}

/// The parenthesised end of a header: what the call reports, then the
/// directory it runs in. The directory is set in italics so it never reads as
/// one more thing the call reported, and it goes last so it holds its place
/// when a result puts an exit status in front of it.
pub(super) fn annotation_spans(
    report: Option<&str>,
    workdir: Option<String>,
) -> Vec<Span<'static>> {
    let style = theme::current().tool_annotation;
    let Some(workdir) = workdir else {
        return report
            .map(|report| {
                vec![Span::styled(
                    format!("{ANNOTATION_OPEN}{report}{ANNOTATION_CLOSE}"),
                    style,
                )]
            })
            .unwrap_or_default();
    };
    let lead = match report {
        Some(report) => format!("{ANNOTATION_OPEN}{report}{ACTIVITY_SEPARATOR}"),
        None => ANNOTATION_OPEN.to_owned(),
    };
    vec![
        Span::styled(lead, style),
        Span::styled(workdir, style.add_modifier(Modifier::ITALIC)),
        Span::styled(ANNOTATION_CLOSE, style),
    ]
}

/// Moves an annotation onto a header row and the row's copy together, so the
/// row copies as the characters it shows.
fn push_annotation(
    spans: &mut Vec<Span<'static>>,
    copy: &mut String,
    annotation: Vec<Span<'static>>,
) {
    copy.extend(annotation.iter().map(|span| span.content.as_ref()));
    spans.extend(annotation);
}

pub(crate) fn append_annotation(ann: &mut Option<String>, suffix: &str) {
    match ann {
        Some(a) => write!(a, "{ACTIVITY_SEPARATOR}{suffix}").unwrap(),
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
    b.push_header(
        "load",
        header,
        annotation_spans(annotation.as_deref(), None),
        None,
        None,
        None,
    );
    b.prepend_indicator(Instant::now());

    let output = Arc::new(ToolOutput::Instructions {
        blocks: blocks.to_vec(),
    });
    b.limits.width = width;
    b.push_rendered_code(None, Some(&output));

    b.push_search_text(
        &blocks
            .iter()
            .map(|bl| bl.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );

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
        highlight: Vec::new(),
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
    use crate::markdown::{TRUNCATION_PREFIX, truncate_output};
    use crate::provenance::Provenance;
    use crate::selection::ScreenSelection;
    use caudra_agent::tools::{
        BATCH_TOOL_NAME, FILE_GREP_TOOL_NAME, FILE_READ_TOOL_NAME, SHELL_TOOL_NAME, TASK_TOOL_NAME,
        ToolEffect,
    };
    use caudra_agent::{
        BatchToolEntry, BatchToolStatus, GrepFileEntry, GrepMatchGroup, ShellFilterInfo,
        SnapshotLine, SnapshotSpan, SubagentActivity, TextOutput, ToolInput, ToolOutput,
    };
    use std::time::Duration;
    use test_case::test_case;

    static NO_VIEWS: std::sync::LazyLock<BatchViewMap> =
        std::sync::LazyLock::new(BatchViewMap::new);
    static NO_PROGRESS: std::sync::LazyLock<BatchProgressMap> =
        std::sync::LazyLock::new(BatchProgressMap::new);
    static NO_LIVE: std::sync::LazyLock<BatchLiveMap> = std::sync::LazyLock::new(BatchLiveMap::new);
    static NO_STARTED: std::sync::LazyLock<BatchStartedMap> =
        std::sync::LazyLock::new(BatchStartedMap::new);

    /// Wide enough that no card drawn in these tests has to break a row, for
    /// the tests whose subject is what a row says rather than how it breaks.
    const UNBROKEN: u16 = 400;

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
            batch_started: &NO_STARTED,
            task_cards: None,
            cwd: None,
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
            tool_stage: None,
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
            tool_started: None,
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
        assert_eq!(!tl.highlight.is_empty(), expect_highlight);
        assert_eq!(
            tl.highlight
                .iter()
                .any(|request| request.sources().1.is_some()),
            expect_output
        );
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
            tool_stage: None,
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
            tool_started: None,
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

    const MEMORY_NOTE: &str = "render-loop-perf.md";
    /// A note whose name the extension rule cannot judge, which is the case
    /// the tool has to answer for.
    const UNEXTENDED_NOTE: &str = "draft";

    fn memory_msg(header: &str, live_body: Option<&str>, status: ToolStatus) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status,
                name: MEMORY_TOOL_NAME.into(),
            })),
            text: header.into(),
            ..write_msg(header, live_body, None)
        }
    }

    fn open_card(msg: &DisplayMessage, status: ToolStatus) -> String {
        lines_text(&build_tool_lines(
            msg,
            status,
            &test_rctx(80),
            Some(Disclosure::default()),
        ))
    }

    const REPORT_TITLE: &str = "Validation finding";
    const REPORT_MESSAGE: &str = "\n\n# Finding\n\nThe **important** detail.\n\nLast report line.";
    const REPORT_ACK: &str = "Report durably recorded; no reply is expected.";
    const REPORT_ERROR: &str = "Report could not be recorded.";
    const REPORT_COPY_MESSAGE: &str = "# Finding\n\n- **Important** detail\n\n```rust\nlet ready = true;\n```\n\nAfter the fence.";

    fn report_msg(status: ToolStatus, raw_input: Option<Value>) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "report".into(),
                effect: ToolEffect::Unknown,
                status,
                name: REPORT_TOOL_NAME.into(),
            })),
            tool_raw_input: raw_input.map(Arc::new),
            ..bash_msg(
                REPORT_TOOL_NAME,
                status,
                None,
                match status {
                    ToolStatus::Success => Some(ToolOutput::Plain(REPORT_ACK.into())),
                    ToolStatus::Error => Some(ToolOutput::Plain(REPORT_ERROR.into())),
                    ToolStatus::InProgress => None,
                },
            )
        }
    }

    fn opened_report_card(
        raw: Value,
        batch: bool,
        compact: bool,
        width: u16,
    ) -> (Vec<Line<'static>>, Option<BodySource>) {
        if batch {
            let output = ToolOutput::Batch {
                entries: vec![BatchToolEntry {
                    model_suffix: None,
                    tool: REPORT_TOOL_NAME.into(),
                    effect: ToolEffect::Unknown,
                    summary: REPORT_TOOL_NAME.into(),
                    status: BatchToolStatus::Success,
                    input: None,
                    raw_input: Some(raw),
                    output: Some(ToolOutput::Plain(REPORT_ACK.into())),
                    annotation: None,
                }],
                text: String::new(),
            };
            let content = code_view::render_tool_content(
                None,
                Some(&output),
                false,
                RenderLimits::new(true, usize::MAX, BatchViews::new([0]), TOL)
                    .with_width(width)
                    .with_policy(
                        CardPolicy {
                            compact,
                            ..CardPolicy::default()
                        },
                        Arc::default(),
                    ),
            );
            (content.lines, content.source)
        } else {
            let card = build_tool_lines(
                &report_msg(ToolStatus::Success, Some(raw)),
                ToolStatus::Success,
                &RenderCtx {
                    compact,
                    ..test_rctx(width)
                },
                Some(exp(true)),
            );
            (card.lines, card.source)
        }
    }

    #[test_case(false, false, 24; "standalone_narrow_lf")]
    #[test_case(false, true, 24; "standalone_narrow_crlf")]
    #[test_case(false, false, UNBROKEN; "standalone_wide_lf")]
    #[test_case(false, true, UNBROKEN; "standalone_wide_crlf")]
    #[test_case(true, false, 24; "batch_narrow_lf")]
    #[test_case(true, true, 24; "batch_narrow_crlf")]
    #[test_case(true, false, UNBROKEN; "batch_wide_lf")]
    #[test_case(true, true, UNBROKEN; "batch_wide_crlf")]
    fn report_cards_copy_markdown_source(batch: bool, crlf: bool, width: u16) {
        let message = if crlf {
            REPORT_COPY_MESSAGE.replace('\n', "\r\n")
        } else {
            REPORT_COPY_MESSAGE.to_owned()
        };
        for compact in [false, true] {
            let (lines, source) = opened_report_card(
                serde_json::json!({
                    "title": REPORT_TITLE,
                    "message": message,
                }),
                batch,
                compact,
                width,
            );
            let source = source.expect("report source provenance");
            assert!(source.text.contains(REPORT_COPY_MESSAGE), "{}", source.text);
            assert_eq!(source.rows.len(), lines.len());
            for (row, line) in source.rows.iter().zip(&lines) {
                assert_eq!(row.spans.len(), line.spans.len());
            }
            let selection = ScreenSelection {
                start_row: 0,
                start_col: 0,
                end_row: lines.len() as u16 - 1,
                end_col: width - 1,
            };
            let copied = Provenance::new(source.text.into(), source.rows)
                .extract(&lines, width, &selection, 0, lines.len() as u16)
                .expect("report selection uses source");
            for fragment in [
                "# Finding",
                "**Important**",
                "```rust",
                "let ready = true;",
                "After the fence.",
                REPORT_ACK,
            ] {
                assert!(copied.contains(fragment), "{copied}");
            }
            assert!(!copied.contains("\\r"), "{copied}");
            let drawn = lines.iter().map(line_text).collect::<String>();
            assert!(!drawn.contains("```"), "{drawn}");
            assert!(!drawn.contains("\\r"), "{drawn}");
        }
    }

    #[test_case(false, 60; "standalone_sixty")]
    #[test_case(false, REPORT_TITLE_MAX_CHARS; "standalone_eighty")]
    #[test_case(true, 60; "batch_sixty")]
    #[test_case(true, REPORT_TITLE_MAX_CHARS; "batch_eighty")]
    fn report_cards_preserve_valid_long_titles(batch: bool, length: usize) {
        let title = "界".repeat(length);
        for compact in [false, true] {
            let (lines, _) = opened_report_card(
                serde_json::json!({
                    "title": title,
                    "message": REPORT_MESSAGE,
                }),
                batch,
                compact,
                UNBROKEN,
            );
            let header = line_text(&lines[0]);
            assert!(header.contains(&title), "{header}");
            assert!(!header.contains(ELLIPSIS), "{header}");
        }
    }

    #[test_case("title"; "malformed_title")]
    #[test_case("message"; "legacy_message")]
    fn report_titles_still_bound_oversized_input(key: &str) {
        let raw = serde_json::json!({key: "界".repeat(REPORT_TITLE_MAX_CHARS + 1)});
        let header = report_header(REPORT_TOOL_NAME, REPORT_TOOL_NAME, Some(&raw));
        assert_eq!(
            header,
            format!("{}{ELLIPSIS}", "界".repeat(REPORT_TITLE_MAX_CHARS))
        );
    }

    #[test_case("first\r\nsecond", "first\nsecond"; "normalize_crlf")]
    #[test_case("first\rsecond", "first\\rsecond"; "escape_bare_carriage_return")]
    fn report_markdown_distinguishes_newlines_from_controls(message: &str, expected: &str) {
        assert_eq!(report_markdown(message), expected);
    }

    #[test_case(Some(serde_json::json!({"title": REPORT_TITLE, "message": REPORT_MESSAGE, "blocked": false})), REPORT_TOOL_NAME, REPORT_TITLE; "explicit_title")]
    #[test_case(Some(serde_json::json!({"message": REPORT_MESSAGE})), REPORT_TOOL_NAME, "# Finding"; "restored_legacy_message")]
    #[test_case(Some(serde_json::json!({"title": " \n ", "message": REPORT_MESSAGE})), REPORT_TOOL_NAME, "# Finding"; "empty_title_falls_back")]
    #[test_case(Some(serde_json::json!({"title": 4, "message": false})), REPORT_TOOL_NAME, ""; "malformed_input")]
    #[test_case(Some(Value::Null), REPORT_TOOL_NAME, ""; "null_input")]
    #[test_case(None, REPORT_TOOL_NAME, ""; "missing_input")]
    #[test_case(None, REPORT_TITLE, REPORT_TITLE; "preserve_meaningful_header")]
    #[test_case(Some(serde_json::json!({"blocked": true})), REPORT_TOOL_NAME, REPORT_BLOCKED; "blocked_without_message")]
    fn report_titles_recover_persisted_input(raw: Option<Value>, header: &str, expected: &str) {
        assert_eq!(
            report_header(REPORT_TOOL_NAME, header, raw.as_ref()),
            expected
        );
    }

    #[test_case(false, ToolStatus::Success, "Reported", REPORT_ACK; "normal_success")]
    #[test_case(true, ToolStatus::Success, "Reported", REPORT_ACK; "compact_success")]
    #[test_case(false, ToolStatus::Error, "Report", REPORT_ERROR; "normal_error")]
    #[test_case(true, ToolStatus::Error, "Report", REPORT_ERROR; "compact_error")]
    #[test_case(false, ToolStatus::InProgress, "Reporting", ""; "normal_running")]
    #[test_case(true, ToolStatus::InProgress, "Reporting", ""; "compact_running")]
    fn report_cards_show_message_and_actual_status(
        compact: bool,
        status: ToolStatus,
        label: &str,
        acknowledgement: &str,
    ) {
        let msg = report_msg(
            status,
            Some(serde_json::json!({
                "title": REPORT_TITLE,
                "message": REPORT_MESSAGE,
                "blocked": true,
            })),
        );
        let rctx = RenderCtx {
            compact,
            ..test_rctx(UNBROKEN)
        };
        for expansion in [None, Some(exp(false)), Some(exp(true))] {
            let card = build_tool_lines(&msg, status, &rctx, expansion);
            let text = lines_text(&card);
            assert!(
                text.contains(&format!(
                    "{label} {REPORT_BLOCKED}{ACTIVITY_SEPARATOR}{REPORT_TITLE}"
                )),
                "{text}"
            );
            assert!(!text.contains(REPORT_TOOL_NAME), "{text}");
            assert!(!text.contains("message="), "{text}");
            assert!(!text.contains("title="), "{text}");
            assert!(!text.contains("blocked="), "{text}");
            assert_eq!(
                text.contains("Last report line."),
                expansion.is_some(),
                "{text}"
            );
            assert!(!text.contains("**important**"), "{text}");
            if expansion.is_some() {
                assert!(text.contains("important"), "{text}");
                assert!(card.search_text.contains(REPORT_MESSAGE));
                if !acknowledgement.is_empty() {
                    assert_eq!(text.matches(acknowledgement).count(), 1, "{text}");
                }
            } else {
                assert!(card.truncation);
            }
            if !matches!(status, ToolStatus::Success) {
                assert!(!text.contains("Reported"), "{text}");
                assert!(!text.contains(REPORT_ACK), "{text}");
            }
        }
    }

    #[test_case(false; "normal")]
    #[test_case(true; "compact")]
    fn report_cards_without_input_keep_the_result(compact: bool) {
        let msg = report_msg(ToolStatus::Error, None);
        let card = build_tool_lines(
            &msg,
            ToolStatus::Error,
            &RenderCtx {
                compact,
                ..test_rctx(UNBROKEN)
            },
            Some(exp(true)),
        );
        let text = lines_text(&card);
        assert!(text.contains(REPORT_ERROR), "{text}");
        assert!(!text.contains(REPORT_TOOL_NAME), "{text}");
    }

    #[test_case(false; "normal")]
    #[test_case(true; "compact")]
    fn report_cards_restore_full_messages_over_stale_snapshots(compact: bool) {
        let message = REPORT_MESSAGE.repeat(TOL.get(REPORT_TOOL_NAME) + 1);
        let mut msg = report_msg(
            ToolStatus::Success,
            Some(serde_json::json!({
                "title": REPORT_TITLE,
                "message": message,
            })),
        );
        let snapshot = make_snapshot(vec![vec![SnapshotSpan {
            text: REPORT_TOOL_NAME.into(),
            style: SpanStyle::Named("tool".into()),
        }]]);
        msg.render_header = Some(snapshot.clone());
        msg.render_snapshot = Some(snapshot);
        let card = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &RenderCtx {
                compact,
                ..test_rctx(UNBROKEN)
            },
            Some(exp(true)),
        );
        let text = lines_text(&card);
        assert_eq!(
            text.matches("Last report line.").count(),
            TOL.get(REPORT_TOOL_NAME) + 1
        );
        assert_eq!(text.matches(REPORT_ACK).count(), 1);
        assert!(!text.contains(REPORT_TOOL_NAME), "{text}");
        assert!(!card.truncation);
    }

    #[test_case(false, 1; "normal_minimal")]
    #[test_case(true, 1; "compact_minimal")]
    #[test_case(false, 24; "normal_narrow")]
    #[test_case(true, 24; "compact_narrow")]
    fn report_cards_bound_titles_and_escape_controls(compact: bool, width: u16) {
        let raw = serde_json::json!({
            "title": format!("\u{1b}[31m{}\nsecond title line", "界".repeat(200)),
            "message": "# Finding\n\ncontrol \u{1b}[31m\u{7} text",
            "blocked": true,
        });
        let header = report_header(REPORT_TOOL_NAME, REPORT_TOOL_NAME, Some(&raw));
        assert!(header.contains(ELLIPSIS));
        assert!(!header.contains("second title line"));
        let msg = report_msg(ToolStatus::Success, Some(raw));
        for expansion in [None, Some(exp(true))] {
            let card = build_tool_lines(
                &msg,
                ToolStatus::Success,
                &RenderCtx {
                    compact,
                    ..test_rctx(width)
                },
                expansion,
            );
            assert!(!lines_text(&card).chars().any(char::is_control));
            assert_eq!(card.links.rows.len(), card.lines.len());
            assert_eq!(card.rows.len(), card.lines.len());
        }
    }

    /// A note settles into a rendered document, so the body drawn while it
    /// arrives has to be that document too. Its header is a sub-command and
    /// an opaque name, so the tool answers for it rather than the extension.
    #[test_case(MEMORY_NOTE ; "a_name_the_extension_rule_would_pass")]
    #[test_case(UNEXTENDED_NOTE ; "a_name_the_extension_rule_would_fail")]
    fn a_streaming_note_is_drawn_as_a_document(note: &str) {
        let msg = memory_msg(
            &format!("write {note}"),
            Some(HEADING_SOURCE),
            ToolStatus::InProgress,
        );
        let text = open_card(&msg, ToolStatus::InProgress);
        assert!(text.contains(HEADING_TEXT), "{text}");
        assert!(!text.contains(HEADING_SOURCE), "{text}");
    }

    /// A store's verb is an argument, so the label cannot carry the tense and
    /// the header does. The past tense asserts the call happened, which is why
    /// a failed write claims only the plain verb.
    #[test_case(ToolStatus::InProgress, "writing" ; "in_flight")]
    #[test_case(ToolStatus::Success,    "wrote"   ; "landed")]
    #[test_case(ToolStatus::Error,      "write"   ; "failed_claims_nothing")]
    fn a_store_header_conjugates_its_own_verb(status: ToolStatus, expected: &str) {
        let msg = memory_msg(&format!("write {MEMORY_NOTE}"), None, status);
        let text = open_card(&msg, status);
        assert!(
            text.contains(&format!("{expected} {MEMORY_NOTE}")),
            "{text}"
        );
    }

    /// The conjugated header no longer contains the raw `command` value, so
    /// the containment check cannot fold it and the row would repeat the verb
    /// it just finished inflecting.
    #[test]
    fn a_conjugated_header_does_not_repeat_its_command_in_brackets() {
        let raw = serde_json::json!({
            "command": "write",
            "path": MEMORY_NOTE,
            "content": HEADING_SOURCE,
        });
        assert_eq!(
            compact_args_for(
                MEMORY_TOOL_NAME,
                &format!("wrote {MEMORY_NOTE}"),
                Some(&raw),
                None
            ),
            None
        );
    }

    const NOTE_ONCE_MSG: &str = "a note is drawn from its structure, and only from it";

    /// A browse used to settle as `ToolOutput::Markdown`, which put its text
    /// into the card's own `msg.text` as well. Drawing the structure while
    /// that text was still there would draw every note twice.
    #[test]
    fn a_settled_note_is_drawn_once() {
        const NOTE_TEXT: &str = "the one body";
        let mut msg = memory_msg(&format!("read {MEMORY_NOTE}"), None, ToolStatus::Success);
        msg.tool_output = Some(Arc::new(ToolOutput::Memory(
            caudra_agent::MemoryOutput::Notes {
                directory: None,
                notes: Vec::from([caudra_agent::MemoryNote {
                    name: MEMORY_NOTE.into(),
                    tokens: 1,
                    tags: Vec::new(),
                    origin: caudra_agent::MemoryOrigin::File {
                        path: format!("/notes/{MEMORY_NOTE}"),
                    },
                    body: NOTE_TEXT.into(),
                }]),
                notices: Vec::new(),
            },
        )));

        let text = open_card(&msg, ToolStatus::Success);

        assert_eq!(
            text.matches(NOTE_TEXT).count(),
            1,
            "{NOTE_ONCE_MSG}: {text}"
        );
        assert_eq!(
            text.matches(MEMORY_NOTE).count(),
            2,
            "the header names it, and so does its own row: {text}"
        );
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
        assert_eq!(!tl.highlight.is_empty(), keeps_markers);
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

    /// A prompt as the model writes it: prose, several lines, and long enough
    /// that a header could only ever hold a prefix of it.
    const STREAMING_PROMPT: &str =
        "A wide cinematic shot of a lighthouse\nat dusk, storm clouds behind it";
    const GENERATED_PATH: &str = "assets/hero.png";
    const LIVE_PROMPT_MSG: &str = "a generation draws the prompt it is being told, line by line, \
        and keeps drawing the same body once the call stamps it";
    const PROMPT_NAMES_NO_ROW_MSG: &str =
        "a generation's row names the file it writes, which its prompt never repeats";

    fn image_msg(live: Option<&str>, input: Option<ToolInput>) -> DisplayMessage {
        let mut msg = bash_msg(GENERATED_PATH, ToolStatus::InProgress, input, None);
        if let DisplayRole::Tool(tool) = &mut msg.role {
            tool.name = IMAGE_GENERATE_TOOL_NAME.into();
        }
        msg.live_body = live.map(str::to_owned);
        msg
    }

    fn prompt_input() -> Option<ToolInput> {
        Some(ToolInput::Code {
            language: "markdown".into(),
            code: STREAMING_PROMPT.into(),
        })
    }

    /// The reported bug: a generation drew nothing at all while its prompt
    /// streamed, and the whole prompt then jumped into the header's
    /// parentheses when the call started. The body it streams is the body it
    /// keeps, so the two frames draw the same card.
    #[test]
    fn a_generation_draws_one_prompt_from_the_first_token_to_the_call() {
        let open = |msg| {
            lines_text(&build_tool_lines(
                &msg,
                ToolStatus::InProgress,
                &test_rctx(80),
                Some(Disclosure::default()),
            ))
        };

        let streaming = open(image_msg(Some(STREAMING_PROMPT), None));

        for line in STREAMING_PROMPT.lines() {
            assert!(streaming.contains(line), "{LIVE_PROMPT_MSG}: {streaming:?}");
        }
        assert_eq!(
            streaming,
            open(image_msg(None, prompt_input())),
            "{LIVE_PROMPT_MSG}"
        );
    }

    /// A command defers its header to the body about to spell it out. A
    /// generation must not: the path and the prompt are different things, and
    /// dropping the path would leave the card unable to say what it writes.
    #[test]
    fn a_streaming_generation_keeps_the_path_on_its_row() {
        let tl = build_tool_lines(
            &image_msg(Some(STREAMING_PROMPT), None),
            ToolStatus::InProgress,
            &test_rctx(80),
            Some(Disclosure::default()),
        );

        assert!(
            line_text(&tl.lines[0]).contains(GENERATED_PATH),
            "{PROMPT_NAMES_NO_ROW_MSG}: {}",
            line_text(&tl.lines[0])
        );
    }

    /// Routing the prompt through the live *body* would clip it to the height
    /// the output gets and let it jump to full length once the call starts.
    #[test]
    fn a_streaming_prompt_is_drawn_whole_rather_than_windowed() {
        const WINDOW: u32 = 1;
        let window = ScrollWindow {
            height: WINDOW as usize,
            offset: 0,
            follow: true,
        };

        let tl = build_tool_lines(
            &image_msg(Some(STREAMING_PROMPT), None),
            ToolStatus::InProgress,
            &scroll_rctx(80, WINDOW, window),
            Some(Disclosure::default()),
        );

        let text = lines_text(&tl);
        for line in STREAMING_PROMPT.lines() {
            assert!(text.contains(line), "{LIVE_PROMPT_MSG}: {text:?}");
        }
        assert!(!tl.truncation, "{LIVE_PROMPT_MSG}: {text:?}");
    }

    const STAGED_TITLE_MSG: &str = "a call that has not run names the stage it is in where its \
        verb would stand, and keeps the header beside it";

    /// The same card open and folded to its row: the two must not call one
    /// call two things.
    fn staged_titles(tool: &str, stage: Option<CallStage>) -> [String; 2] {
        let mut msg = image_msg(None, None);
        if let DisplayRole::Tool(role) = &mut msg.role {
            role.name = tool.into();
        }
        msg.tool_stage = stage;
        [
            build_tool_lines(
                &msg,
                ToolStatus::InProgress,
                &test_rctx(80),
                Some(Disclosure::default()),
            ),
            build_tool_lines(&msg, ToolStatus::InProgress, &compact_rctx(80), None),
        ]
        .map(|tl| line_text(&tl.lines[0]))
    }

    #[test_case(
        IMAGE_GENERATE_TOOL_NAME,
        Some(CallStage::Drafting),
        WRITING_PROMPT
        ; "a_prompt_being_written"
    )]
    #[test_case(
        IMAGE_GENERATE_TOOL_NAME,
        Some(CallStage::AwaitingApproval),
        AWAITING_APPROVAL
        ; "a_generation_awaiting_approval"
    )]
    #[test_case(IMAGE_GENERATE_TOOL_NAME, None, DRAW.1 ; "a_generation_running")]
    #[test_case(SHELL_TOOL_NAME, Some(CallStage::Drafting), WRITING_COMMAND ; "a_command_being_written")]
    #[test_case(
        PYTHON_EXECUTION_TOOL_NAME,
        Some(CallStage::Drafting),
        WRITING_SCRIPT
        ; "a_script_being_written"
    )]
    #[test_case(TASK_TOOL_NAME, Some(CallStage::Drafting), WRITING_BRIEF ; "a_brief_being_written")]
    #[test_case(
        FILE_READ_TOOL_NAME,
        Some(CallStage::Drafting),
        READ.1
        ; "a_call_too_short_to_watch_being_written_keeps_its_verb"
    )]
    #[test_case(
        FILE_READ_TOOL_NAME,
        Some(CallStage::AwaitingApproval),
        AWAITING_APPROVAL
        ; "any_call_awaiting_approval"
    )]
    fn a_staged_card_names_its_stage(tool: &str, stage: Option<CallStage>, label: &str) {
        let expected = format!("{label} {GENERATED_PATH}");
        for title in staged_titles(tool, stage) {
            assert!(title.contains(&expected), "{STAGED_TITLE_MSG}: {title:?}");
        }
    }

    /// A store's header opens on the verb the call asks for, and a call still
    /// waiting to be allowed has only asked.
    #[test]
    fn a_staged_store_call_reads_as_the_request_it_is() {
        let mut msg = memory_msg(
            &format!("write {MEMORY_NOTE}"),
            None,
            ToolStatus::InProgress,
        );
        msg.tool_stage = Some(CallStage::AwaitingApproval);

        let text = open_card(&msg, ToolStatus::InProgress);

        assert!(
            text.contains(&format!("{AWAITING_APPROVAL} write {MEMORY_NOTE}")),
            "{STAGED_TITLE_MSG}: {text:?}"
        );
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
            &test_rctx(UNBROKEN),
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

    #[test_case("{\"output\": {\"note\": \"**literal**\"}}"; "complete")]
    #[test_case("{\"output\": {\"note\": \"**literal**"; "truncated")]
    fn structured_task_previews_remain_json(preview: &str) {
        const LITERAL: &str = "**literal**";
        let markdown = readable_task_preview(preview);
        assert!(markdown.starts_with("```json\n"));
        let (lines, _) = task_card::markdown_body(&markdown, 80);
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(LITERAL));
        assert!(!text.contains("```"));
    }

    #[test_case(false; "literal_stdout_and_stderr")]
    #[test_case(true; "filtered_literal_output")]
    fn shell_task_details_use_typed_shell_projection(filtered: bool) {
        const RAW: &str = "**literal stdout**";
        const FILTERED: &str = "`literal filtered output`";
        let ToolOutput::Shell(mut shell) = shell_output(filtered) else {
            unreachable!()
        };
        shell.stdout = RAW.into();
        shell.model_text = FILTERED.into();
        let expected = if filtered {
            shell.model_text.clone()
        } else {
            shell.raw_text()
        };
        let card: TaskCard = serde_json::from_value(serde_json::json!({
            "kind": "shell", "task_id": "shell-task", "invocation_id": "invocation",
            "call_id": "shell-call", "root_call_id": "shell-call", "label": "Print", "state": "succeeded",
            "mode": "build", "background": true, "generation": 1, "created_at": 1, "updated_at": 2,
            "result": {"output": "model output", "shell": ToolOutput::Shell(shell)},
        })).unwrap();
        assert_eq!(task_details(&card), expected);
        let (lines, _) = task_card::details(&card, UNBROKEN);
        let rendered = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(if filtered { FILTERED } else { RAW }));
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

    #[test_case(false, false; "foreground")]
    #[test_case(false, true; "restored_foreground")]
    #[test_case(true, false; "batch")]
    #[test_case(true, true; "restored_batch")]
    fn typed_task_markdown_keeps_links_and_row_targets(batched: bool, restored: bool) {
        const LINK: &str = "https://example.com/task";
        const BODY: &str = "**Finding**: [documentation](https://example.com/task)\n\n- checked";
        let task: TaskCard = serde_json::from_value(serde_json::json!({
            "task_id": "readable-task", "invocation_id": "invocation", "call_id": "call",
            "root_call_id": "call", "label": "Inspect", "state": "succeeded", "mode": "build",
            "background": false, "generation": 1, "created_at": 1, "updated_at": 2,
            "result": {"output": BODY}
        }))
        .unwrap();
        let output = ToolOutput::Tasks(vec![task]);
        let output = if batched {
            ToolOutput::Batch {
                entries: vec![BatchToolEntry {
                    tool: TASK_TOOL_NAME.into(),
                    effect: ToolEffect::Unknown,
                    summary: String::new(),
                    status: BatchToolStatus::Success,
                    input: code_input(),
                    raw_input: None,
                    output: Some(output),
                    annotation: None,
                    model_suffix: None,
                }],
                text: BODY.into(),
            }
        } else {
            output
        };
        let output = if restored {
            serde_json::from_value(serde_json::to_value(output).unwrap()).unwrap()
        } else {
            output
        };
        let mut msg = task_msg(String::new());
        if !batched {
            msg.tool_input = code_input().map(Arc::new);
        }
        msg.tool_output = Some(Arc::new(output));
        let views = HashMap::from([("t1".into(), BatchViews::new([0]))]);
        let ctx = RenderCtx {
            batch_views: &views,
            ..test_rctx(48)
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &ctx,
            Some(Disclosure {
                full: true,
                ..Default::default()
            }),
        );
        assert!(!lines_text(&tl).contains("**Finding**"));
        assert!(tl.search_text.contains("Finding"));
        assert!(tl.links.is_aligned(&tl.lines));
        let (row, span) = tl
            .links
            .rows
            .iter()
            .enumerate()
            .find_map(|(row, links)| {
                links
                    .iter()
                    .position(|link| link.as_deref() == Some(LINK))
                    .map(|span| (row, span))
            })
            .unwrap();
        let column = tl.lines[row].spans[..span]
            .iter()
            .map(Span::width)
            .sum::<usize>() as u16;
        assert_eq!(
            tl.links
                .target_at(&tl.lines, 48, row as u16, column)
                .as_deref(),
            Some(LINK)
        );
        assert!(tl.rows[row].is_some());
        assert!(tl.lines.iter().all(|line| line.width() <= 48));
        assert_eq!(tl.highlight.len(), 1);
        for request in &tl.highlight {
            let (input, output) = request.sources();
            assert!(output.is_none());
            let rendered = request.region.render(input, output);
            assert_eq!(
                rendered.lines.iter().map(line_text).collect::<Vec<_>>(),
                tl.lines[request.region.range.clone()]
                    .iter()
                    .map(line_text)
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            tl.highlight
                .iter()
                .all(|request| !request.region.range.contains(&row))
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
            tool_stage: None,
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
            tool_started: None,
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
            tool_stage: None,
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
            tool_started: None,
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
            tool_stage: None,
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
            tool_started: None,
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
        assert_eq!(resolved.dropped, 42);
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
            tool_stage: None,
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
            tool_started: None,
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

    const WINDOW_ROWS_MSG: &str = "a window is a height in terminal rows, so a card drawn in one \
        keeps that height however long the lines arriving under it are";
    const WINDOW_OFFSET_MSG: &str =
        "a window offset counts the rows the body was painted into, before and after it grew";
    const NARROW_BODY_MSG: &str = "a narrow card still draws its body across the width it has";
    /// Long enough to take several terminal rows at every width these cases
    /// use, so a window counted in source lines and one counted in rows cannot
    /// agree by accident.
    const WRAPPING_PAD: usize = 90;

    fn wrapping_output(count: usize) -> String {
        (0..count)
            .map(|index| format!("row{index}-{}", "x".repeat(WRAPPING_PAD)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn scroll_card(count: usize, width: u16, window: ScrollWindow) -> ToolLines {
        let msg = bash_msg(
            "cmd",
            ToolStatus::Success,
            None,
            Some(ToolOutput::Plain(wrapping_output(count).into())),
        );
        build_tool_lines(
            &msg,
            ToolStatus::Success,
            &scroll_rctx(width, SCROLL_HEIGHT, window),
            Some(exp(false)),
        )
    }

    /// The rows the card gave its window, and the height of the whole card.
    fn windowed(tl: &ToolLines, msg: &str) -> (Vec<String>, usize) {
        let span = tl.scroll_spans.first().copied().expect(msg);
        let rows = tl.lines[span.first..span.first + span.lines]
            .iter()
            .map(line_text)
            .collect();
        (rows, tl.lines.len())
    }

    /// The reported defect: a card with a fixed line budget grew and shrank as
    /// output arrived, because the budget picked source lines while the card
    /// was painted in the rows those lines wrapped into.
    #[test_case(40 ; "narrow")]
    #[test_case(80 ; "wide")]
    fn a_windowed_card_paints_the_same_rows_however_long_its_lines_are(width: u16) {
        let measured: Vec<(usize, usize)> = [6usize, 9, 20, 61]
            .into_iter()
            .map(|count| {
                let tl = scroll_card(count, width, at(0, true));
                let (rows, height) = windowed(&tl, WINDOW_ROWS_MSG);
                (rows.len(), height)
            })
            .collect();

        assert_eq!(measured[0].0, SCROLL_HEIGHT as usize, "{WINDOW_ROWS_MSG}");
        assert!(
            measured.iter().all(|seen| *seen == measured[0]),
            "{WINDOW_ROWS_MSG}: {measured:?}"
        );
    }

    /// A paused window holds a position in the rows the card painted. Counted
    /// in source lines it lands somewhere else entirely, and moves again as
    /// soon as the lines under it wrap differently.
    #[test_case(40 ; "narrow")]
    #[test_case(80 ; "wide")]
    fn a_paused_window_holds_the_rows_the_body_was_painted_into(width: u16) {
        const OFFSET: usize = 7;
        const COUNT: usize = 30;
        let (painted, _) =
            code_view::plain_body(&wrapping_output(COUNT), width - TOOL_BODY_INDENT_WIDTH);
        let expected: Vec<String> = painted[OFFSET..OFFSET + SCROLL_HEIGHT as usize]
            .iter()
            .map(|line| format!("{TOOL_BODY_INDENT}{}", line_text(line)))
            .collect();

        let tl = scroll_card(COUNT, width, at(OFFSET, false));
        let grown = scroll_card(COUNT * 2, width, at(OFFSET, false));

        let (shown, _) = windowed(&tl, WINDOW_OFFSET_MSG);
        assert_eq!(shown, expected, "{WINDOW_OFFSET_MSG}");
        assert_eq!(
            windowed(&grown, WINDOW_OFFSET_MSG).0,
            expected,
            "{WINDOW_OFFSET_MSG}"
        );
        let span = tl.scroll_spans[0];
        assert_eq!((span.offset, span.total), (OFFSET, painted.len()));
        assert_eq!(span.history_start, None, "{WINDOW_OFFSET_MSG}");
    }

    /// Holding a card still is worth nothing if it costs the body the columns
    /// it is read in, which is what a narrow terminal has least of.
    #[test_case(40 ; "forty")]
    #[test_case(30 ; "thirty")]
    #[test_case(24 ; "twenty_four")]
    fn a_narrow_windowed_card_still_draws_its_body(width: u16) {
        let tl = scroll_card(20, width, at(0, true));

        let (shown, _) = windowed(&tl, NARROW_BODY_MSG);
        assert_eq!(shown.len(), SCROLL_HEIGHT as usize, "{NARROW_BODY_MSG}");
        for row in &shown {
            let body = row.strip_prefix(TOOL_BODY_INDENT).unwrap_or(row);
            assert!(!body.trim().is_empty(), "{NARROW_BODY_MSG}: {row:?}");
        }
        let widest = shown.iter().map(|row| row.chars().count()).max();
        assert_eq!(widest, Some(usize::from(width)), "{NARROW_BODY_MSG}");
    }

    const SNAPSHOT_SPAN_MSG: &str = "a snapshot card drawing the footer must publish the window that footer describes, or \
         there is nothing for a bar to sit beside and the wheel falls through to the transcript";
    const SNAPSHOT_TRACK_MSG: &str = "a window's track is the rows it painted, so a snapshot line the card had to break \
         lengthens the track instead of leaving it counting lines";

    /// Whoever painted a snapshot laid it out already, so its lines reach the
    /// card unbroken and the card's own final break is what splits them. That
    /// makes it the one body whose window is recorded across fewer lines than
    /// it paints rows.
    fn wide_snapshot(count: usize) -> BufferSnapshot {
        make_snapshot(
            (0..count)
                .map(|index| {
                    vec![SnapshotSpan {
                        text: format!("row{index}-{}", "y".repeat(WRAPPING_PAD)),
                        style: SpanStyle::Default,
                    }]
                })
                .collect(),
        )
    }

    /// The reported defect: every tool streaming through the live buffer draws
    /// its window's footer from a snapshot, and the card published no span to
    /// go with it, so the bar had nowhere to land and the wheel scrolled the
    /// transcript out from under the card the reader was pointing at.
    #[test_case(40 ; "narrow")]
    #[test_case(80 ; "wide")]
    fn a_windowed_snapshot_card_publishes_the_window_it_painted(width: u16) {
        const TOTAL: usize = 40;
        let shown = SCROLL_HEIGHT as usize;
        let tl = build_tool_lines(
            &snapshot_msg(wide_snapshot(TOTAL)),
            ToolStatus::InProgress,
            &scroll_rctx(width, SCROLL_HEIGHT, at(0, true)),
            Some(exp(false)),
        );

        let base = tl.snapshot_base.expect(SNAPSHOT_SPAN_MSG);
        let footer = tl.scroll_footer_line.expect(SNAPSHOT_SPAN_MSG);
        let span = tl.scroll_spans.first().copied().expect(SNAPSHOT_SPAN_MSG);

        assert_eq!(span.first, base, "{SNAPSHOT_SPAN_MSG}");
        assert_eq!(span.extent_lines, shown, "{SNAPSHOT_SPAN_MSG}");
        assert_eq!(
            (span.offset, span.total),
            (TOTAL - shown, TOTAL),
            "{SNAPSHOT_SPAN_MSG}"
        );
        // The footer is pushed straight after the rows the window kept, so it
        // is exactly where the track has to stop.
        assert_eq!(span.first + span.lines, footer, "{SNAPSHOT_TRACK_MSG}");
        assert!(
            span.lines > shown,
            "{SNAPSHOT_TRACK_MSG}: {} rows for {shown} lines",
            span.lines
        );
    }

    #[test_case(80, 4; "two_source_rows_paint_four_rows")]
    #[test_case(40, 6; "two_source_rows_paint_six_rows")]
    fn a_snapshot_scroll_extent_stays_in_the_offset_units(width: u16, painted: usize) {
        const TOTAL: usize = 5;
        const SELECTED: u32 = 2;
        let shown = SELECTED as usize;
        let tl = build_tool_lines(
            &snapshot_msg(wide_snapshot(TOTAL)),
            ToolStatus::InProgress,
            &scroll_rctx(
                width,
                SELECTED,
                ScrollWindow {
                    height: shown,
                    offset: 0,
                    follow: true,
                },
            ),
            Some(exp(false)),
        );
        let span = tl.scroll_spans.first().copied().expect(SNAPSHOT_SPAN_MSG);
        assert_eq!(
            (span.total, span.offset, span.extent_lines, span.lines),
            (TOTAL, TOTAL - shown, shown, painted),
            "{SNAPSHOT_TRACK_MSG}"
        );
        assert_eq!(
            span.total.saturating_sub(span.extent_lines),
            span.offset,
            "{SNAPSHOT_SPAN_MSG}"
        );
    }

    const BUDGET_ROWS_MSG: &str = "a budget is a height in terminal rows too, so a card resting \
        at one keeps that height however long the lines arriving under it are";
    const BUDGET_NOTICE_MSG: &str = "the notice counts what the budget counts, so the rows it \
        names and the rows the body left out are the same number";
    const BUDGET_WHOLE_MSG: &str =
        "a body that fits its budget is drawn whole, and never padded out to fill it";
    const BUDGET_TAIL_MSG: &str = "a command is read from its newest output back, so a budget \
        spent on its rows is spent from the bottom up";
    const LONG_LINE_MSG: &str = "one line longer than the whole budget must still show its tail \
        rather than vanishing or taking more rows than the budget has";
    const NARROW_BUDGET_MSG: &str =
        "a card held to a budget still draws its body across the width it has";

    /// Raised off the `bash` default so a budget card has rows to lose and
    /// still say something with the ones it keeps.
    const BUDGET: usize = 10;
    /// The notice's own row comes out of the budget, so an abridged body is
    /// one row shorter than an unabridged one is allowed to be.
    const BUDGET_BODY_ROWS: usize = BUDGET - 1;
    /// These fixtures head their card with one short command, which no width
    /// under test breaks.
    const HEADER_ROWS: usize = 1;

    const BUDGET_TOL: ToolOutputLines = ToolOutputLines {
        bash: BUDGET,
        ..ToolOutputLines::DEFAULT
    };

    fn budget_rctx(width: u16) -> RenderCtx<'static> {
        RenderCtx {
            tool_output_lines: &BUDGET_TOL,
            ..test_rctx(width)
        }
    }

    const SHELL_FIXTURE_MSG: &str = "the shell fixture is what gives a card the tail-keeping \
        budget, so nothing else can stand in for it";

    /// A command's own output, with nothing for the shell footer to report, so
    /// the notice stays the last row the card pushes.
    fn budget_output(text: String, tail: bool) -> ToolOutput {
        if !tail {
            return ToolOutput::Plain(text.into());
        }
        let ToolOutput::Shell(base) = shell_output(false) else {
            unreachable!("{SHELL_FIXTURE_MSG}")
        };
        ToolOutput::Shell(ShellOutput {
            stdout: text,
            stderr: String::new(),
            ..base
        })
    }

    fn budget_card(text: String, tail: bool, width: u16) -> ToolLines {
        let msg = bash_msg(
            "cmd",
            ToolStatus::Success,
            None,
            Some(budget_output(text, tail)),
        );
        build_tool_lines(
            &msg,
            ToolStatus::Success,
            &budget_rctx(width),
            Some(exp(false)),
        )
    }

    /// Every row the body would paint if nothing held it back, which is the
    /// only thing a claim about withheld rows can be checked against.
    fn painted_rows(text: &str, width: u16) -> Vec<String> {
        code_view::plain_body(text, width - TOOL_BODY_INDENT_WIDTH)
            .0
            .iter()
            .map(|line| format!("{TOOL_BODY_INDENT}{}", line_text(line)))
            .collect()
    }

    /// The rows a card gave its body, and the count its notice claims. The
    /// notice is the last thing pushed, so the body is what lies between it
    /// and the header.
    fn budgeted(tl: &ToolLines) -> (Vec<String>, Option<usize>) {
        let rows: Vec<String> = tl.lines.iter().map(line_text).collect();
        let notice = rows.iter().position(|row| row.contains(TRUNCATION_PREFIX));
        let body = rows[HEADER_ROWS..notice.unwrap_or(rows.len())].to_vec();
        (body, notice.map(|at| notice_count(&rows[at..])))
    }

    /// A narrow card breaks the notice across rows, so the number is read back
    /// off the whole of it rather than off the row it started on.
    fn notice_count(notice: &[String]) -> usize {
        let text = notice.join("");
        let (_, count) = text.split_once('(').expect(BUDGET_NOTICE_MSG);
        count
            .split_whitespace()
            .next()
            .expect(BUDGET_NOTICE_MSG)
            .parse()
            .expect(BUDGET_NOTICE_MSG)
    }

    /// The reported defect on the path a fixed budget takes: the card's height
    /// moved with the length of the lines in it, because the budget picked
    /// source lines while the card was painted in the rows they wrapped into.
    #[test_case(80, false ; "wide_head")]
    #[test_case(80, true  ; "wide_tail")]
    #[test_case(40, false ; "narrow_head")]
    #[test_case(40, true  ; "narrow_tail")]
    fn a_budgeted_card_paints_the_same_rows_however_long_its_lines_are(width: u16, tail: bool) {
        let measured: Vec<usize> = [6usize, 9, 20, 61]
            .into_iter()
            .map(|count| {
                budgeted(&budget_card(wrapping_output(count), tail, width))
                    .0
                    .len()
            })
            .collect();

        assert!(
            measured.iter().all(|rows| *rows == BUDGET_BODY_ROWS),
            "{BUDGET_ROWS_MSG}: {measured:?}"
        );
    }

    /// A notice counting lines while the body spends rows is a notice that
    /// disagrees with the card the moment anything wraps.
    #[test_case(80, false ; "wide_head")]
    #[test_case(80, true  ; "wide_tail")]
    #[test_case(40, false ; "narrow_head")]
    #[test_case(40, true  ; "narrow_tail")]
    #[test_case(24, true  ; "very_narrow_tail")]
    fn a_budget_notice_counts_the_rows_it_withheld(width: u16, tail: bool) {
        const COUNT: usize = 20;
        let text = wrapping_output(COUNT);
        let painted = painted_rows(&text, width);

        let (body, claimed) = budgeted(&budget_card(text, tail, width));

        assert_eq!(
            claimed,
            Some(painted.len() - body.len()),
            "{BUDGET_NOTICE_MSG}"
        );
    }

    /// The budget is a ceiling, not a height to reach: a short body keeps the
    /// rows it has and the card ends there.
    #[test_case(80 ; "wide")]
    #[test_case(24 ; "very_narrow")]
    fn a_body_inside_its_budget_is_drawn_whole(width: u16) {
        const COUNT: usize = 2;
        let text = wrapping_output(COUNT);
        let painted = painted_rows(&text, width);

        let (body, claimed) = budgeted(&budget_card(text, false, width));

        assert!(painted.len() <= BUDGET, "{BUDGET_WHOLE_MSG}: {painted:?}");
        assert_eq!(body, painted, "{BUDGET_WHOLE_MSG}");
        assert_eq!(claimed, None, "{BUDGET_WHOLE_MSG}");
    }

    /// A command that printed for a minute is read from the end, so the rows
    /// the budget keeps have to be the last ones painted rather than the
    /// first.
    #[test_case(80 ; "wide")]
    #[test_case(40 ; "narrow")]
    fn a_command_budget_keeps_the_rows_its_newest_output_painted(width: u16) {
        const COUNT: usize = 20;
        let text = wrapping_output(COUNT);
        let painted = painted_rows(&text, width);

        let (body, _) = budgeted(&budget_card(text, true, width));

        assert_eq!(
            body,
            painted[painted.len() - body.len()..],
            "{BUDGET_TAIL_MSG}"
        );
    }

    /// A body whose every row belongs to one source line is the case a budget
    /// counted in lines cannot express at all: keeping the line keeps all of
    /// it, and dropping it leaves the card with nothing.
    #[test_case(80 ; "wide")]
    #[test_case(40 ; "narrow")]
    #[test_case(24 ; "very_narrow")]
    fn one_line_too_long_for_the_budget_still_shows_its_tail(width: u16) {
        const LONG_LINE_CHARS: usize = 1_200;
        let text = format!("tail-of-one-long-line-{}", "z".repeat(LONG_LINE_CHARS));
        let painted = painted_rows(&text, width);

        let (body, claimed) = budgeted(&budget_card(text, true, width));

        assert!(painted.len() > BUDGET, "{LONG_LINE_MSG}: {}", painted.len());
        assert_eq!(body.len(), BUDGET_BODY_ROWS, "{LONG_LINE_MSG}");
        assert_eq!(
            body,
            painted[painted.len() - body.len()..],
            "{LONG_LINE_MSG}"
        );
        assert_eq!(claimed, Some(painted.len() - body.len()), "{LONG_LINE_MSG}");
    }

    /// Holding a card still is worth nothing if it costs the body the columns
    /// it is read in, which is what a narrow terminal has least of.
    #[test_case(40 ; "forty")]
    #[test_case(30 ; "thirty")]
    #[test_case(24 ; "twenty_four")]
    fn a_narrow_budgeted_card_still_draws_its_body(width: u16) {
        let (body, _) = budgeted(&budget_card(wrapping_output(20), false, width));

        assert_eq!(body.len(), BUDGET_BODY_ROWS, "{NARROW_BUDGET_MSG}");
        for row in &body {
            let drawn = row.strip_prefix(TOOL_BODY_INDENT).unwrap_or(row);
            assert!(!drawn.trim().is_empty(), "{NARROW_BUDGET_MSG}: {row:?}");
        }
        let widest = body.iter().map(|row| row.chars().count()).max();
        assert_eq!(widest, Some(usize::from(width)), "{NARROW_BUDGET_MSG}");
    }

    const DROPPED_MSG: &str = "rows cut before the card saw them are rows it cannot paint, so the \
        notice adds them to what it withheld itself rather than reporting one and losing the other";

    #[test]
    fn a_budget_notice_counts_what_never_reached_the_card_too() {
        const DROPPED: usize = 7;
        const WIDTH: u16 = 40;
        let text = wrapping_output(20);
        let painted = painted_rows(&text, WIDTH);
        let mut msg = bash_msg("cmd", ToolStatus::Success, None, None);
        msg.text = format!("cmd\n{text}");
        msg.truncated_lines = DROPPED;

        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &budget_rctx(WIDTH),
            Some(exp(false)),
        );

        let (body, claimed) = budgeted(&tl);
        assert_eq!(
            claimed,
            Some(painted.len() - body.len() + DROPPED),
            "{DROPPED_MSG}"
        );
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
            tool_stage: None,
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
            tool_started: None,
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
        assert!(!tl.highlight.is_empty());
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

    /// Back-dated far enough that the tenths are stable however slow the test
    /// host is, and a magnitude the settled formatter spells differently.
    const SHELL_RAN_FOR: Duration = Duration::from_millis(1_201);
    const LIVE_CLOCK: &str = " · 1.2s";
    /// What `shell_output()` reports as the command's own time.
    const MEASURED_CLOCK: &str = " · 10ms";

    fn running_shell(started: Option<Duration>) -> DisplayMessage {
        let mut msg = bash_msg("cargo test", ToolStatus::InProgress, None, None);
        msg.tool_started = started.map(|ago| Instant::now() - ago);
        msg
    }

    #[test_case(test_rctx(80)    ; "expanded")]
    #[test_case(compact_rctx(80) ; "compact")]
    fn a_running_shell_header_carries_a_live_clock(rctx: RenderCtx<'static>) {
        let msg = running_shell(Some(SHELL_RAN_FOR));
        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &rctx, None);

        assert!(
            line_text(&tl.lines[0]).ends_with(LIVE_CLOCK),
            "header should end with the live clock: {}",
            line_text(&tl.lines[0])
        );
    }

    #[test]
    fn a_settled_shell_header_reports_the_commands_own_time() {
        let mut msg = bash_msg(
            "cargo test",
            ToolStatus::Success,
            None,
            Some(shell_output(false)),
        );
        msg.tool_started = Some(Instant::now() - SHELL_RAN_FOR);
        let tl = build_tool_lines(&msg, ToolStatus::Success, &test_rctx(80), None);

        let header = line_text(&tl.lines[0]);
        assert!(header.ends_with(MEASURED_CLOCK), "got {header}");
        assert!(
            !header.contains(LIVE_CLOCK),
            "the subprocess's own time supersedes the wall clock: {header}"
        );
    }

    #[test]
    fn an_unstarted_shell_card_draws_no_clock() {
        let msg = running_shell(None);
        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &test_rctx(80), None);

        assert!(!line_text(&tl.lines[0]).contains(DURATION_SEPARATOR));
    }

    fn code_child() -> BatchToolEntry {
        BatchToolEntry {
            tool: FILE_READ_TOOL_NAME.into(),
            effect: ToolEffect::Unknown,
            summary: SOURCE_PATH.into(),
            status: BatchToolStatus::Success,
            input: None,
            raw_input: None,
            output: Some(ToolOutput::ReadCode {
                path: SOURCE_PATH.into(),
                start_line: 1,
                lines: vec!["fn main() {}".into()],
                total_lines: 1,
                instructions: None,
            }),
            annotation: None,
            model_suffix: None,
        }
    }

    fn running_shell_child() -> BatchToolEntry {
        BatchToolEntry {
            tool: SHELL_TOOL_NAME.into(),
            status: BatchToolStatus::Running,
            output: None,
            ..code_child()
        }
    }

    fn batch_of(entries: Vec<BatchToolEntry>, started: &BatchStartedMap) -> ToolLines {
        let mut msg = bash_msg("batch", ToolStatus::InProgress, None, None);
        if let DisplayRole::Tool(tool) = &mut msg.role {
            tool.name = BATCH_TOOL_NAME.into();
        }
        msg.tool_output = Some(Arc::new(ToolOutput::Batch {
            entries,
            text: String::new(),
        }));
        let rctx = RenderCtx {
            batch_started: started,
            ..test_rctx(80)
        };
        build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &rctx,
            Some(Disclosure::default()),
        )
    }

    #[test]
    fn a_batch_with_a_ticking_child_highlights_only_stable_regions() {
        let clocked = BatchStartedMap::from([(
            "t1".to_owned(),
            Arc::new(HashMap::from([(1_usize, Instant::now())])),
        )]);
        let settled = batch_of(vec![code_child()], &BatchStartedMap::new());
        let ticking = batch_of(vec![code_child(), running_shell_child()], &clocked);
        assert_eq!(settled.highlight.len(), 1);
        assert_eq!(ticking.highlight.len(), 1);
        let request = &ticking.highlight[0];
        assert_eq!(request.region.path, [0]);
        let (input, output) = request.sources();
        let result = request.region.render(input, output);
        assert_eq!(
            result.lines.iter().map(line_text).collect::<Vec<_>>(),
            ticking.lines[request.region.range.clone()]
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
        );
        assert!(
            !result
                .lines
                .iter()
                .any(|line| line_text(line).contains(DURATION_SEPARATOR))
        );
    }

    /// Same card, same start time: only `shell` reports a duration of its own,
    /// so only `shell` gets a clock.
    #[test]
    fn a_non_shell_tool_draws_no_clock() {
        let mut msg = running_shell(Some(SHELL_RAN_FOR));
        if let DisplayRole::Tool(tool) = &mut msg.role {
            tool.name = FILE_READ_TOOL_NAME.into();
        }
        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &test_rctx(80), None);

        assert!(!line_text(&tl.lines[0]).contains(DURATION_SEPARATOR));
    }

    const SUBAGENT_ELAPSED: Duration = Duration::from_millis(63_400);

    fn report(activity: SubagentActivity, tools: u32) -> SubagentProgress {
        SubagentProgress {
            activity,
            tools,
            elapsed: SUBAGENT_ELAPSED,
        }
    }

    /// What three tools and `SUBAGENT_ELAPSED` spell, which is what the header
    /// reports for every case built on `running_tool_report(3)`.
    const SUBAGENT_TALLY: &str = "3 tools · 1m 3.4s";
    const TALLY_IN_HEADER_MSG: &str =
        "a subagent's tally sits in the header parentheses, beside what the run spent";
    const SETTLED_HAS_NO_ROW_MSG: &str =
        "a settled subagent is described by its output and its header, so it draws no tree row";

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
    ///
    /// The row says what the subagent is doing; how much it has done is the
    /// header's to report, in the parentheses it already keeps for the spend.
    #[test_case(None                        ; "collapsed")]
    #[test_case(Some(Disclosure::default()) ; "expanded")]
    fn a_running_subagent_reports_its_tool_under_the_header(expansion: Option<Disclosure>) {
        let msg = subagent_msg(ToolStatus::InProgress, Some(running_tool_report(3)));

        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &test_rctx(80), expansion);

        assert_eq!(tl.lines.len(), 2, "{}", lines_text(&tl));
        assert_eq!(line_text(&tl.lines[1]), "  └── $ Running cargo nextest run");
        assert!(
            line_text(&tl.lines[0]).contains(&format!("({SUBAGENT_TALLY})")),
            "{TALLY_IN_HEADER_MSG}: {}",
            line_text(&tl.lines[0])
        );
    }

    const CLAMP_ROW_MSG: &str = "a live row must fit without splitting display characters";
    const SETTLED_TALLY_MSG: &str = "a settled activity tally wraps instead of losing detail";
    const SETTLED_TALLY_WIDTH: u16 = 16;

    #[test_case("abcdef", 0, "abcdef"; "unconstrained")]
    #[test_case("abc", 3, "abc"; "exact_fit")]
    #[test_case("abcd", 3, "ab…"; "ascii")]
    #[test_case("abcd", 1, "…"; "one_column")]
    #[test_case("漢字x", 3, "漢…"; "wide_characters")]
    #[test_case("\u{2764}\u{fe0f}x", 2, "…"; "presentation_selector")]
    #[test_case("a\u{301}bc", 2, "a\u{301}…"; "combining_mark")]
    fn a_live_status_row_fits_its_display_width(text: &str, width: u16, expected: &str) {
        let style = Style::default().fg(Color::Red);
        let row = Line::from(clamp_to_row(
            vec![Span::styled(text.to_owned(), style)],
            width,
        ));
        let drawn = line_text(&row);
        assert_eq!(drawn, expected, "{CLAMP_ROW_MSG}");
        assert!(
            width == UNCONSTRAINED_WIDTH || drawn.width() <= usize::from(width),
            "{CLAMP_ROW_MSG}"
        );
        assert!(
            row.spans.iter().all(|span| span.style == style),
            "{CLAMP_ROW_MSG}"
        );
    }

    #[test_case(ToolStatus::Success; "success")]
    #[test_case(ToolStatus::Error; "error")]
    fn a_settled_subagent_wraps_its_complete_tally(status: ToolStatus) {
        let msg = subagent_msg(status, Some(running_tool_report(7)));
        let tl = build_tool_lines(
            &msg,
            status,
            &test_rctx(SETTLED_TALLY_WIDTH),
            Some(Disclosure::default()),
        );
        let text = lines_text(&tl);
        assert!(text.contains("7 tools"), "{SETTLED_TALLY_MSG}: {text}");
        assert!(text.contains("3.4s"), "{SETTLED_TALLY_MSG}: {text}");
        assert!(!text.contains(ELLIPSIS), "{SETTLED_TALLY_MSG}: {text}");
    }

    const NESTED_ROSTER_MSG: &str =
        "a subagent batching draws the roster it is working through, one node in";
    const ROSTER_ONE_ROW: &str = "a roster row must stay one row however long the call it names, \
        or the tree's height tracks whatever the subagent happens to be running";
    /// Long enough that a narrow card has to break the roster row drawing it.
    const LONG_SUMMARY: &str =
        "cargo nextest run --workspace --locked --no-fail-fast --status-level all";
    const ROSTER_WIDTH: u16 = 40;

    fn batch_child(tool: &str, summary: &str, status: BatchToolStatus) -> ActivityChild {
        ActivityChild {
            tool: Arc::from(tool),
            summary: summary.to_owned(),
            status,
        }
    }

    const HISTORY_FIRST: &str = "first batch";
    const HISTORY_SECOND: &str = "second batch";
    const HISTORY_READ: &str = "history.rs";
    const HISTORY_RUN: &str = "history command";
    const HISTORY_GREP: &str = "history pattern";
    const HISTORY_THINKING: &str = "Thinking";
    /// The same phase once a later activity has replaced it.
    const HISTORY_THOUGHT: &str = "Thought";
    const HISTORY_TOOLS: u32 = 7;
    /// The tally as an activity row used to carry it. Nothing spells this now,
    /// which is what the history cases assert.
    const HISTORY_TALLY: &str = " · 7 tools · ";
    /// The tally as the header opens it. Stops short of the clock, which runs
    /// while the case does.
    const HISTORY_HEADER_TALLY: &str = "(7 tools · ";
    const HISTORY_WINDOW: u32 = 4;
    const HISTORY_WIDTH: u16 = 80;
    const HISTORY_ROWS: usize = 8;
    const HISTORY_OUTPUT: &str = "output before the history\noutput beside the history";
    const HISTORY_ANSWER: &str = "the settled answer";
    const HISTORY_TREE_MSG: &str = "retained batches keep their statuses and continuing trunks";
    const HISTORY_CURRENT_MSG: &str = "the current phase appears exactly once, and the tally it \
        used to carry is the header's to report";
    const HISTORY_WINDOW_MSG: &str = "one task body owns output, history, and one scroll span";
    const HISTORY_WORKER_MSG: &str = "live history must not be overwritten by a highlight result";
    const HISTORY_LINK_MSG: &str =
        "windowing progress and output keeps markdown links on their rows";
    const HISTORY_LINK: &str = "https://example.com/history";

    fn retained_task_progress() -> ToolProgress {
        let mut progress = ToolProgress::live(report(
            SubagentActivity::batch(
                Arc::from(BATCH_TOOL_NAME),
                HISTORY_FIRST,
                vec![
                    batch_child(FILE_READ_TOOL_NAME, HISTORY_READ, BatchToolStatus::Running),
                    batch_child(SHELL_TOOL_NAME, HISTORY_RUN, BatchToolStatus::Error),
                    batch_child(FILE_GREP_TOOL_NAME, HISTORY_GREP, BatchToolStatus::Pending),
                ],
            ),
            HISTORY_TOOLS,
        ));
        progress.update(report(
            SubagentActivity::Thinking { title: None },
            HISTORY_TOOLS,
        ));
        progress.update(report(
            SubagentActivity::batch(
                Arc::from(BATCH_TOOL_NAME),
                HISTORY_SECOND,
                vec![batch_child(
                    SHELL_TOOL_NAME,
                    LONG_SUMMARY,
                    BatchToolStatus::Running,
                )],
            ),
            HISTORY_TOOLS,
        ));
        progress.update(report(
            SubagentActivity::Thinking { title: None },
            HISTORY_TOOLS,
        ));
        progress
    }

    fn history_msg() -> DisplayMessage {
        let mut msg = subagent_msg(ToolStatus::InProgress, None);
        if let DisplayRole::Tool(tool) = &mut msg.role {
            tool.name = TASK_TOOL_NAME.into();
        }
        msg.progress = Some(retained_task_progress());
        msg
    }

    fn history_text(line: &Line<'static>) -> String {
        line_text(line)
            .split(ACTIVITY_SEPARATOR)
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    #[test_case(TOOL_BODY_INDENT; "standalone")]
    #[test_case(TREE_TRUNK; "nested_with_sibling")]
    #[test_case(TREE_GAP; "nested_last_child")]
    fn retained_groups_draw_real_rows_and_one_current_phase(continuation: &str) {
        let progress = retained_task_progress();
        let lines = progress_lines(&progress, continuation, UNBROKEN);
        let rows: Vec<_> = lines.iter().map(history_text).collect();
        assert_eq!(
            rows,
            [
                format!("{continuation}{TREE_BRANCH}⇶ Batched {HISTORY_FIRST}"),
                format!("{continuation}{TREE_TRUNK}{TREE_BRANCH}→ Reading {HISTORY_READ}"),
                format!("{continuation}{TREE_TRUNK}{TREE_BRANCH}$ Run {HISTORY_RUN}"),
                format!("{continuation}{TREE_TRUNK}{TREE_LAST}⌕ Grep {HISTORY_GREP}"),
                format!("{continuation}{TREE_BRANCH}{HISTORY_THOUGHT}"),
                format!("{continuation}{TREE_BRANCH}⇶ Batched {HISTORY_SECOND}"),
                format!("{continuation}{TREE_TRUNK}{TREE_LAST}$ Running {LONG_SUMMARY}"),
                format!("{continuation}{TREE_LAST}{HISTORY_THINKING}"),
            ],
            "{HISTORY_TREE_MSG}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line_text(line).contains(HISTORY_TALLY)),
            "{HISTORY_CURRENT_MSG}"
        );
        assert_eq!(
            lines[2].spans[1].style,
            batch_sigil_style(BatchToolStatus::Error, None),
            "{HISTORY_TREE_MSG}"
        );
    }

    #[test_case(48; "narrow")]
    #[test_case(HISTORY_WIDTH; "wide")]
    fn standalone_history_and_output_use_one_row_window(width: u16) {
        let mut msg = history_msg();
        msg.live_body = Some(format!("[history link]({HISTORY_LINK})"));
        msg.live_output = Some(wrapping_output(2));
        let whole = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &test_rctx(width),
            Some(exp(true)),
        );
        let windowed = |window| {
            build_tool_lines(
                &msg,
                ToolStatus::InProgress,
                &scroll_rctx(width, HISTORY_WINDOW, window),
                Some(Disclosure::default()),
            )
        };
        let header_rows = windowed(ScrollWindow {
            height: HISTORY_WINDOW as usize,
            offset: 0,
            follow: false,
        })
        .scroll_spans[0]
            .first;
        let expected: Vec<_> = whole
            .lines
            .iter()
            .skip(header_rows)
            .map(history_text)
            .collect();
        let history_start = expected.len() - HISTORY_ROWS;
        assert!(whole.scroll_spans.is_empty(), "{HISTORY_WINDOW_MSG}");
        for offset in 0..expected.len() {
            let window = ScrollWindow {
                height: HISTORY_WINDOW as usize,
                offset,
                follow: false,
            };
            let (start, end) = window.range(expected.len());
            let tl = windowed(window);
            assert_eq!(tl.scroll_spans.len(), 1, "{HISTORY_WINDOW_MSG}");
            let span = tl.scroll_spans[0];
            assert_eq!(span.extent_lines, span.lines, "{HISTORY_WINDOW_MSG}");
            assert_eq!(
                span.history_start,
                Some(history_start),
                "{HISTORY_WINDOW_MSG}"
            );
            assert_eq!(
                (span.child, span.first, span.lines, span.total, span.offset),
                (None, header_rows, end - start, expected.len(), start),
                "{HISTORY_WINDOW_MSG}"
            );
            let shown: Vec<_> = tl.lines[span.first..span.first + span.lines]
                .iter()
                .map(history_text)
                .collect();
            assert_eq!(shown, expected[start..end], "{HISTORY_WINDOW_MSG}");
            assert_eq!(
                tl.scroll_footer_line,
                Some(span.first + span.lines),
                "{HISTORY_WINDOW_MSG}"
            );
            assert_eq!(tl.rows.len(), tl.lines.len(), "{HISTORY_WINDOW_MSG}");
            assert_eq!(
                tl.links.rows[span.first..span.first + span.lines],
                whole.links.rows[start + header_rows..end + header_rows],
                "{HISTORY_LINK_MSG}"
            );
            assert!(tl.highlight.is_empty(), "{HISTORY_WORKER_MSG}");
        }
        assert!(
            whole
                .links
                .rows
                .iter()
                .flatten()
                .any(|link| link.as_deref() == Some(HISTORY_LINK)),
            "{HISTORY_LINK_MSG}"
        );
    }

    #[test_case(0; "unwindowed")]
    #[test_case(HISTORY_WINDOW; "windowed")]
    fn collapsed_task_output_keeps_retained_status_visible(height: u32) {
        let mut msg = history_msg();
        msg.live_output = Some(HISTORY_OUTPUT.to_owned());
        let rctx = RenderCtx {
            policy: CardPolicy {
                scroll_card_lines: height,
                always_collapsed: Arc::from([TASK_TOOL_NAME.to_owned()]),
                ..CardPolicy::default()
            },
            ..test_rctx(HISTORY_WIDTH)
        };
        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &rctx, None);
        let text = lines_text(&tl);
        assert!(
            HISTORY_OUTPUT.lines().all(|line| !text.contains(line)),
            "{HISTORY_WINDOW_MSG}: {text}"
        );
        assert_eq!(
            text.matches(HISTORY_THINKING).count(),
            1,
            "{HISTORY_CURRENT_MSG}"
        );
        assert_eq!(
            text.matches(HISTORY_HEADER_TALLY).count(),
            1,
            "{HISTORY_CURRENT_MSG}"
        );
        if height == 0 {
            assert_eq!(tl.lines.len(), HISTORY_ROWS + 1, "{HISTORY_WINDOW_MSG}");
            assert!(tl.scroll_spans.is_empty(), "{HISTORY_WINDOW_MSG}");
        } else {
            assert_eq!(tl.scroll_spans.len(), 1, "{HISTORY_WINDOW_MSG}");
            assert_eq!(
                tl.scroll_spans[0].history_start,
                Some(0),
                "{HISTORY_WINDOW_MSG}"
            );
            assert_eq!(
                tl.scroll_spans[0].total, HISTORY_ROWS,
                "{HISTORY_WINDOW_MSG}"
            );
        }
    }

    #[test_case(false; "closed")]
    #[test_case(true; "open")]
    fn compact_task_history_follows_body_disclosure(open: bool) {
        let tl = build_tool_lines(
            &history_msg(),
            ToolStatus::InProgress,
            &compact_rctx(UNBROKEN),
            open.then(Disclosure::default),
        );
        let text = lines_text(&tl);
        assert_eq!(
            tl.lines.len(),
            if open { HISTORY_ROWS + 1 } else { 1 },
            "{HISTORY_WINDOW_MSG}"
        );
        assert_eq!(
            text.matches(HISTORY_THINKING).count(),
            1,
            "{HISTORY_CURRENT_MSG}"
        );
        assert_eq!(
            text.matches(HISTORY_HEADER_TALLY).count(),
            1,
            "{HISTORY_CURRENT_MSG}"
        );
        assert_eq!(text.contains(HISTORY_FIRST), open, "{HISTORY_TREE_MSG}");
        assert!(tl.scroll_spans.is_empty(), "{HISTORY_WINDOW_MSG}");
    }

    #[test]
    fn retained_history_does_not_block_stable_input_highlighting() {
        let limits = RenderLimits {
            progress: Arc::new(HashMap::from([(0, retained_task_progress())])),
            width: HISTORY_WIDTH,
            ..RenderLimits::default()
        };
        let input = code_input().unwrap();
        let output = ToolOutput::Batch {
            entries: vec![code_child()],
            text: String::new(),
        };
        let content = code_view::render_tool_content(Some(&input), Some(&output), false, limits);
        assert!(
            content
                .highlights
                .iter()
                .any(|region| region.path.is_empty() && region.role == CodeRole::Input)
        );
        for region in content.highlights {
            assert!(
                !content.lines[region.range]
                    .iter()
                    .any(|line| line_text(line).contains(HISTORY_FIRST))
            );
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
                "  └── ⇶ Batching 3 tools",
                "      ├── → Read a.rs",
                "      ├── $ Running cargo check",
                "      └── ⌕ Grep fn main",
            ],
            "{NESTED_ROSTER_MSG}"
        );
    }

    const ROSTER_HELD_MSG: &str = "a child changing state rewrites its own row and leaves the \
        roster the height and the order it already had";
    /// The calls one roster names, distinct enough that a row drawn in the
    /// wrong place reads as the wrong call rather than as a changed one.
    const ROSTER_CALLS: [(&str, &str); 3] = [
        (FILE_READ_TOOL_NAME, "a.rs"),
        (SHELL_TOOL_NAME, "cargo check"),
        (FILE_GREP_TOOL_NAME, "fn main"),
    ];

    /// The rows a batching subagent's card draws, with its children in
    /// `states`.
    fn subagent_roster(states: [BatchToolStatus; ROSTER_CALLS.len()]) -> Vec<String> {
        let children = ROSTER_CALLS
            .iter()
            .zip(states)
            .map(|((tool, call), status)| batch_child(tool, call, status))
            .collect();
        let activity = SubagentActivity::batch(Arc::from(BATCH_TOOL_NAME), "3 tools", children);
        let msg = subagent_msg(ToolStatus::InProgress, Some(report(activity, 3)));

        build_tool_lines(&msg, ToolStatus::InProgress, &test_rctx(80), None)
            .lines
            .iter()
            .map(line_text)
            .collect()
    }

    /// The same reservation one level up from the batch card: the call named
    /// its children when it parsed, so one of them running and then answering
    /// rewrites a row and moves none.
    #[test]
    fn a_batching_subagent_holds_its_roster_as_its_children_move() {
        let queued = subagent_roster([BatchToolStatus::Pending; ROSTER_CALLS.len()]);
        let underway = subagent_roster([
            BatchToolStatus::Success,
            BatchToolStatus::Running,
            BatchToolStatus::Pending,
        ]);

        assert_eq!(
            underway.len(),
            queued.len(),
            "{ROSTER_HELD_MSG}: {underway:?}"
        );
        let roster = &underway[underway.len() - ROSTER_CALLS.len()..];
        for (row, (_, call)) in roster.iter().zip(ROSTER_CALLS) {
            assert!(row.ends_with(call), "{ROSTER_HELD_MSG}: {underway:?}");
        }
    }

    /// The reported bug: a roster row whose summary outgrew the card wrapped,
    /// so the row's height tracked whichever call the subagent had reached,
    /// and every switch between calls of different lengths reflowed the rows
    /// under it. One child is one row, whatever it is running.
    #[test]
    fn a_long_roster_row_stays_one_row() {
        let activity = SubagentActivity::batch(
            Arc::from(BATCH_TOOL_NAME),
            "2 tools",
            vec![
                batch_child(SHELL_TOOL_NAME, LONG_SUMMARY, BatchToolStatus::Running),
                batch_child(FILE_GREP_TOOL_NAME, "fn main", BatchToolStatus::Pending),
            ],
        );
        let msg = subagent_msg(ToolStatus::InProgress, Some(report(activity, 2)));

        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &test_rctx(ROSTER_WIDTH), None);

        let rows: Vec<String> = tl.lines.iter().map(line_text).collect();
        let level = format!("{TOOL_BODY_INDENT}{TREE_GAP}");
        let opened = rows
            .iter()
            .position(|row| row.starts_with(&format!("{level}{TREE_BRANCH}")))
            .expect(ROSTER_ONE_ROW);
        let last = rows
            .iter()
            .position(|row| row.starts_with(&format!("{level}{TREE_LAST}")))
            .expect(ROSTER_ONE_ROW);

        assert_eq!(last, opened + 1, "{ROSTER_ONE_ROW}: {rows:#?}");
        assert!(
            rows[opened].ends_with(ELLIPSIS),
            "{ROSTER_ONE_ROW}: {:?}",
            rows[opened]
        );
    }

    const HEADER_HANG: &str = "a header too long for its card carries on under its label, not \
        under the spinner or the sigil that opened the row";
    /// The indicator and the sigil, which a card puts in front of its label.
    const HEAD_WIDTH: usize = 4;

    /// Nothing about a spinner frame or a tool's sigil says it is chrome, so
    /// the row that puts them there is the only thing that can say so.
    #[test_case(ToolStatus::InProgress ; "running")]
    #[test_case(ToolStatus::Success ; "finished")]
    fn a_wrapped_header_hangs_under_its_label(status: ToolStatus) {
        let msg = bash_msg(LONG_SUMMARY, status, None, None);

        let lines = build_tool_lines(&msg, status, &test_rctx(ROSTER_WIDTH), None);

        let rows: Vec<String> = lines.lines.iter().map(line_text).collect();
        assert!(rows.len() > 1, "{HEADER_HANG}: {rows:#?}");
        let hang = &rows[1];
        assert!(
            hang.starts_with(&" ".repeat(HEAD_WIDTH)),
            "{HEADER_HANG}: {hang:?}"
        );
        assert!(
            !hang.starts_with(&" ".repeat(HEAD_WIDTH + 1)),
            "{HEADER_HANG}: {hang:?}"
        );
    }

    const NOTHING_OVERFLOWS: &str = "a gutter too deep to break into clips what it draws rather \
        than running past the card, where the terminal would break it at column zero and take the \
        tree with it";
    /// Narrow enough that the roster's own gutter leaves less than the columns
    /// a break needs to be worth making.
    const CRAMPED: u16 = 14;

    #[test]
    fn a_row_with_no_room_left_to_break_is_clipped_to_the_card() {
        let activity = SubagentActivity::batch(
            Arc::from(BATCH_TOOL_NAME),
            "2 tools",
            vec![
                batch_child(SHELL_TOOL_NAME, LONG_SUMMARY, BatchToolStatus::Running),
                batch_child(FILE_GREP_TOOL_NAME, LONG_SUMMARY, BatchToolStatus::Pending),
            ],
        );
        let msg = subagent_msg(ToolStatus::InProgress, Some(report(activity, 2)));

        let lines = build_tool_lines(&msg, ToolStatus::InProgress, &test_rctx(CRAMPED), None);

        for row in lines.lines.iter().map(line_text) {
            let drawn = UnicodeWidthStr::width(row.as_str());
            assert!(
                drawn <= usize::from(CRAMPED),
                "{NOTHING_OVERFLOWS}: {row:?}"
            );
        }
    }

    /// The roster describes what the call is doing, so it goes when the call
    /// stops, exactly as the activity beside it does.
    #[test]
    fn a_settled_subagent_drops_the_roster_with_its_activity() {
        let mut msg = history_msg();
        msg.progress.as_mut().expect(HISTORY_TREE_MSG).settle();
        msg.tool_output = Some(Arc::new(ToolOutput::Plain(HISTORY_ANSWER.into())));
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(HISTORY_WIDTH),
            Some(Disclosure::default()),
        );
        let text = lines_text(&tl);
        assert!(!text.contains(HISTORY_FIRST), "{HISTORY_TREE_MSG}: {text}");
        assert!(!text.contains(HISTORY_SECOND), "{HISTORY_TREE_MSG}: {text}");
        assert!(
            !text.contains(HISTORY_THINKING),
            "{HISTORY_CURRENT_MSG}: {text}"
        );
        assert!(
            text.contains(HISTORY_ANSWER),
            "{HISTORY_WINDOW_MSG}: {text}"
        );
        assert_eq!(
            text.matches(&format!("{HISTORY_TOOLS} tools")).count(),
            1,
            "{HISTORY_CURRENT_MSG}: {text}"
        );
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

        assert_eq!(line_text(&tl.lines[1]), "  └── Responding");
    }

    /// What it was doing is stale the moment it stops; what it did is not, and
    /// the header is where it is said.
    #[test_case(ToolStatus::Success ; "success")]
    #[test_case(ToolStatus::Error   ; "error")]
    fn a_settled_subagent_keeps_only_its_tally(status: ToolStatus) {
        let msg = subagent_msg(status, Some(running_tool_report(7)));

        let tl = build_tool_lines(&msg, status, &test_rctx(80), Some(Disclosure::default()));

        let text = lines_text(&tl);
        assert!(
            line_text(&tl.lines[0]).contains("(7 tools · 1m 3.4s)"),
            "{TALLY_IN_HEADER_MSG}: {text}"
        );
        assert!(!text.contains("cargo nextest run"), "{text}");
        assert!(!text.contains('└'), "{SETTLED_HAS_NO_ROW_MSG}: {text}");
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
            line_text(&tl.lines[0]).contains(&format!("({expected})")),
            "{TALLY_IN_HEADER_MSG}: {}",
            lines_text(&tl)
        );
    }

    /// What a dispatch spends, as the usage annotation already spells it.
    const SUBAGENT_SPEND: &str = "12.3k↑ 456↓ Σ$1.500";

    /// The reported bug: the tally sat on its own activity row while the spend
    /// sat in the header, so one call reported itself in two places.
    #[test]
    fn a_header_reads_the_tally_before_the_spend() {
        let mut msg = subagent_msg(ToolStatus::InProgress, Some(running_tool_report(3)));
        msg.annotation = Some(SUBAGENT_SPEND.to_owned());

        let tl = build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &test_rctx(120),
            Some(Disclosure::default()),
        );

        let header = line_text(&tl.lines[0]);
        assert!(
            header.contains(&format!(
                "({SUBAGENT_TALLY}{ACTIVITY_SEPARATOR}{SUBAGENT_SPEND})"
            )),
            "{TALLY_IN_HEADER_MSG}: {header}"
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
    /// header rather than claim a second row. Drawn wide enough to hold the
    /// row, because a row the card has to break is a question about widths and
    /// this one is about where the progress goes.
    #[test]
    fn a_compact_row_keeps_the_progress_on_the_header() {
        let msg = subagent_msg(ToolStatus::InProgress, Some(running_tool_report(3)));

        let tl = build_tool_lines(&msg, ToolStatus::InProgress, &compact_rctx(UNBROKEN), None);

        assert_eq!(tl.lines.len(), 1);
        let text = lines_text(&tl);
        assert!(text.contains(" · $ Running cargo nextest run"), "{text}");
        assert!(
            text.contains(&format!("({SUBAGENT_TALLY})")),
            "{TALLY_IN_HEADER_MSG}: {text}"
        );
    }

    const SIGIL_WIDTH_MSG: &str =
        "a sigil holds one cell, or every label behind it sits a column out";
    const SIGIL_CLASH_MSG: &str = "two tools sharing a sigil is a family, and a family is declared";
    /// The sigils a family deliberately shares: the code graph is one tool with
    /// five verbs, the stores are one store with two, and read, write and patch
    /// each keep the cold members whose operation really is the same. Patching
    /// a document is patching a file that happens to live in a remote store.
    const SHARED_SIGILS: &[char] = &['◇', '▤', '→', '←', '±'];

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

    const LIVE_DOCUMENT_MSG: &str =
        "a body that settles to rendered markdown is drawn as a document while it arrives";
    /// A delegation's header is its description, which is prose and has no
    /// extension to read, so the name is the only thing that can say the body
    /// arriving under it is a document.
    const HEADERLESS_EXTENSION: &str = "Find the auth middleware";

    #[test_case(TASK_TOOL_NAME ; "a_delegation")]
    #[test_case(MEMORY_TOOL_NAME ; "a_note")]
    #[test_case(LOCAL_DOCUMENT_WRITE_TOOL_NAME ; "a_local_document")]
    fn a_document_body_is_drawn_as_one_before_it_settles(tool: &str) {
        assert!(
            draws_live_markdown(tool, HEADERLESS_EXTENSION),
            "{LIVE_DOCUMENT_MSG}: {tool}"
        );
    }

    const ACTIVITY_VERB_MSG: &str = "an activity row names a tool by the verb its card header uses";
    const UNTABLED_TOOL: &str = "mcp_Some_unknown";
    const UNTABLED_MSG: &str = "a tool the table has never heard of answers with itself";
    const PHASE_CASE_MSG: &str = "a phase heads its row like the tool verbs beside it";
    const CONTROL_SUMMARY: &str = "lib.rs\u{1b}[2J";
    const DETAIL_ESCAPE_MSG: &str =
        "a summary is built from tool input, so the row must not pass its controls on";

    const SUPERSEDED_MSG: &str = "a row a later activity replaced is finished, so it reads in the \
        past tense the settled batch rows under it already use";

    #[test_case("shell", "Running" ; "shell")]
    #[test_case("file_read", "Reading" ; "read")]
    #[test_case("file_grep", "Grepping" ; "grep")]
    #[test_case("task", "Delegating" ; "task")]
    fn an_activity_names_its_tool_by_verb(tool: &str, expected: &str) {
        let activity = SubagentActivity::tool(Arc::from(tool), "");
        assert_eq!(
            activity_label(&activity, Tense::Present),
            expected,
            "{ACTIVITY_VERB_MSG}"
        );
    }

    #[test_case("shell", "Ran" ; "shell")]
    #[test_case("file_read", "Read" ; "read")]
    #[test_case("file_grep", "Grepped" ; "grep")]
    #[test_case("task", "Delegated" ; "task")]
    fn a_superseded_activity_names_its_tool_in_the_past(tool: &str, expected: &str) {
        let activity = SubagentActivity::tool(Arc::from(tool), "");
        assert_eq!(
            activity_label(&activity, Tense::Past),
            expected,
            "{SUPERSEDED_MSG}"
        );
    }

    const NEVER_RAN_MSG: &str = "a staged row a later activity replaced names a call that never \
        ran, so it takes the plain verb";

    #[test_case(SHELL_TOOL_NAME, CallStage::Drafting, WRITING_COMMAND ; "a_command_being_written")]
    #[test_case(TASK_TOOL_NAME, CallStage::Drafting, WRITING_BRIEF ; "a_brief_being_written")]
    #[test_case(
        FILE_READ_TOOL_NAME,
        CallStage::Drafting,
        READ.1
        ; "a_call_too_short_to_watch_being_written_keeps_its_verb"
    )]
    #[test_case(
        FILE_READ_TOOL_NAME,
        CallStage::AwaitingApproval,
        AWAITING_APPROVAL
        ; "any_call_awaiting_approval"
    )]
    fn a_staged_activity_row_names_its_stage(tool: &str, stage: CallStage, expected: &str) {
        let activity = SubagentActivity::tool(Arc::from(tool), "").with_stage(Some(stage));
        assert_eq!(
            activity_label(&activity, Tense::Present),
            expected,
            "{STAGED_TITLE_MSG}"
        );
    }

    #[test_case(CallStage::Drafting ; "replaced_while_being_written")]
    #[test_case(CallStage::AwaitingApproval ; "replaced_while_awaiting_approval")]
    fn a_replaced_staged_row_takes_the_plain_verb(stage: CallStage) {
        let activity =
            SubagentActivity::tool(Arc::from(SHELL_TOOL_NAME), "").with_stage(Some(stage));
        assert_eq!(
            activity_label(&activity, Tense::Past),
            RUN.0,
            "{NEVER_RAN_MSG}"
        );
    }

    #[test_case(SubagentActivity::Thinking { title: None }, "Thought" ; "thinking")]
    #[test_case(SubagentActivity::Responding, "Responded" ; "responding")]
    #[test_case(SubagentActivity::Compacting, "Compacted" ; "compacting")]
    #[test_case(SubagentActivity::Retrying, "Retried" ; "retrying")]
    fn a_superseded_phase_reads_in_the_past(activity: SubagentActivity, expected: &str) {
        assert_eq!(
            activity_label(&activity, Tense::Past),
            expected,
            "{SUPERSEDED_MSG}"
        );
    }

    #[test]
    fn an_untabled_tool_answers_with_its_own_name() {
        let activity = SubagentActivity::tool(Arc::from(UNTABLED_TOOL), "");
        assert_eq!(
            activity_label(&activity, Tense::Present),
            UNTABLED_TOOL,
            "{UNTABLED_MSG}"
        );
    }

    #[test]
    fn a_phase_is_capitalised_like_the_verbs_beside_it() {
        assert_eq!(
            activity_label(&SubagentActivity::Responding, Tense::Present),
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
            tool_stage: None,
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
            tool_started: None,
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
            tool_stage: None,
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
            tool_started: None,
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

    /// `timeout` is seconds to a fetch, while a server's own tool may count
    /// the same key in millis. A count is left alone: only the table says a
    /// number is a span of time, which is why an untabled tool cannot turn
    /// one into `2m`.
    ///
    /// The two command runners name their deadline in the annotation instead,
    /// default and all, so a bracket repeating it would say it twice.
    #[test_case(
        "shell", serde_json::json!({ "timeoutSec": 600 }), None
        ; "a command runner leaves its timeout to the annotation"
    )]
    #[test_case(
        "python_execution", serde_json::json!({ "timeoutSec": 5 }), None
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
        "mcp_Shell", serde_json::json!({ "timeoutSec": 120 }), None
        ; "a qualified command runner folds it the same way"
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

    const DEADLINE_MSG: &str = "a command card names the deadline it will run under, typed or not";
    const DEADLINE_ONCE_MSG: &str = "a closed row says the deadline once, in the annotation";
    const DEADLINE_COMMAND: &str = "cargo test";
    const DEADLINE_CODE: &str = "21 * 2";
    const TIMEOUT_WORD: &str = "timeout";
    const TIMEOUT_BRACKET: &str = "[timeoutSec=";
    const SHELL_MAX_SHOWN: &str = "(6h timeout)";
    const HOURS_LONG_TIMEOUT_SECS: u64 = 10_800;
    const SHELL_LONGEST_SECS: u64 = 21_600;
    const PAST_SHELL_LONGEST_SECS: u64 = 86_400;

    fn deadline_msg(tool: &str, raw_input: Option<serde_json::Value>) -> DisplayMessage {
        let mut msg = bash_msg(DEADLINE_COMMAND, ToolStatus::Success, None, None);
        let DisplayRole::Tool(role) = &mut msg.role else {
            unreachable!()
        };
        role.name = tool.into();
        msg.tool_raw_input = raw_input.map(Arc::new);
        msg
    }

    /// The deadline nobody typed is the one most likely to surprise a reader
    /// watching a command sit there, so it is named too. The value is read from
    /// the stored input, which is what a reloaded session still has.
    #[test_case(
        SHELL_TOOL_NAME, Some(serde_json::json!({ "command": DEADLINE_COMMAND })), Some("(2m timeout)")
        ; "an unasked deadline is still the one in force"
    )]
    #[test_case(
        SHELL_TOOL_NAME, None, None
        ; "a card with no stored input promises nothing"
    )]
    #[test_case(
        SHELL_TOOL_NAME, Some(serde_json::json!({ "command": DEADLINE_COMMAND, "timeoutSec": 90 })), Some("(1m30s timeout)")
        ; "an asked deadline is quoted as asked"
    )]
    #[test_case(
        SHELL_TOOL_NAME, Some(serde_json::json!({ "command": DEADLINE_COMMAND, "timeoutSec": HOURS_LONG_TIMEOUT_SECS })), Some("(3h timeout)")
        ; "a deadline hours long is quoted in hours"
    )]
    #[test_case(
        SHELL_TOOL_NAME, Some(serde_json::json!({ "command": DEADLINE_COMMAND, "timeoutSec": SHELL_LONGEST_SECS })), Some(SHELL_MAX_SHOWN)
        ; "the longest wait the shell allows is quoted too"
    )]
    #[test_case(
        SHELL_TOOL_NAME, Some(serde_json::json!({ "command": DEADLINE_COMMAND, "timeoutSec": PAST_SHELL_LONGEST_SECS })), None
        ; "a deadline the executor will refuse is not promised"
    )]
    #[test_case(
        SHELL_TOOL_NAME, Some(serde_json::json!({ "command": DEADLINE_COMMAND, "timeoutSec": 0 })), None
        ; "zero is refused, so it promises nothing"
    )]
    #[test_case(
        PYTHON_EXECUTION_TOOL_NAME, Some(serde_json::json!({ "code": DEADLINE_CODE })), Some("(5s timeout)")
        ; "the code worker names its own default"
    )]
    #[test_case(
        PYTHON_EXECUTION_TOOL_NAME, Some(serde_json::json!({ "code": DEADLINE_CODE, "timeoutSec": 0 })), None
        ; "zero is not a deadline the code worker would take"
    )]
    fn a_command_card_names_the_deadline_it_will_run_under(
        tool: &str,
        raw_input: Option<serde_json::Value>,
        expected: Option<&str>,
    ) {
        let text = lines_text(&build_tool_lines(
            &deadline_msg(tool, raw_input),
            ToolStatus::Success,
            &test_rctx(UNBROKEN),
            Some(Disclosure::default()),
        ));
        match expected {
            Some(shown) => assert!(text.contains(shown), "{DEADLINE_MSG}: {text:?}"),
            None => assert!(!text.contains(TIMEOUT_WORD), "{DEADLINE_MSG}: {text:?}"),
        }
    }

    /// The bracket and the annotation would otherwise both quote the same
    /// number on the one row that has least space for it. A width that has to
    /// break the row still says it whole, rather than trading the deadline for
    /// the space.
    #[test_case(UNBROKEN ; "with room to spare")]
    #[test_case(SETTLED_TALLY_WIDTH ; "and with none")]
    fn a_closed_command_row_does_not_quote_its_deadline_twice(width: u16) {
        let tl = build_tool_lines(
            &deadline_msg(
                SHELL_TOOL_NAME,
                Some(serde_json::json!({
                    "command": DEADLINE_COMMAND,
                    "timeoutSec": SHELL_LONGEST_SECS,
                })),
            ),
            ToolStatus::Success,
            &compact_rctx(width),
            None,
        );
        let text = lines_text(&tl);
        assert!(
            text.contains(SHELL_MAX_SHOWN),
            "{DEADLINE_ONCE_MSG}: {text:?}"
        );
        assert!(
            !text.contains(TIMEOUT_BRACKET),
            "{DEADLINE_ONCE_MSG}: {text:?}"
        );
    }

    const WORKDIR_MSG: &str =
        "a command card names where it runs, unless that is the session's own directory";
    const WORKDIR_ITALIC_MSG: &str =
        "only the directory leans, so it never reads as one more thing the call reported";
    const WORKDIR_ONCE_MSG: &str = "a closed row names its directory once, in the annotation";
    const PROJECT: &str = "/project";
    const SUBDIR: &str = "crates/core";
    const SUBDIR_SHOWN: &str = "crates/core/";
    const SUBDIR_ANNOTATION: &str = " (2m timeout · crates/core/)";
    const WORKDIR_BRACKET: &str = "[workdir=";

    fn in_cwd(rctx: RenderCtx<'static>, cwd: Option<&str>) -> RenderCtx<'static> {
        RenderCtx {
            cwd: cwd.map(|cwd| Arc::from(Path::new(cwd))),
            ..rctx
        }
    }

    fn settled_in(relative_workdir: &str) -> ToolOutput {
        let ToolOutput::Shell(output) = shell_output(false) else {
            unreachable!()
        };
        ToolOutput::Shell(ShellOutput {
            relative_workdir: relative_workdir.into(),
            ..output
        })
    }

    /// Running until it has a result, the way a live card is.
    fn workdir_card(
        workdir: &str,
        output: Option<ToolOutput>,
        rctx: &RenderCtx,
        expansion: Option<Disclosure>,
    ) -> ToolLines {
        let status = match output {
            Some(_) => ToolStatus::Success,
            None => ToolStatus::InProgress,
        };
        let mut msg = deadline_msg(
            SHELL_TOOL_NAME,
            Some(serde_json::json!({ "command": DEADLINE_COMMAND, "workdir": workdir })),
        );
        msg.tool_output = output.map(Arc::new);
        build_tool_lines(&msg, status, rctx, expansion)
    }

    /// A card with only its arguments spells the directory the way its result
    /// will, so the header keeps its words when the call lands. Where the two
    /// disagree, the result is where the call really ran.
    #[test_case(
        SUBDIR, Some(settled_in(SUBDIR)), Some(PROJECT), SUBDIR_ANNOTATION
        ; "a settled card names its directory last"
    )]
    #[test_case(
        "", Some(settled_in(CURRENT_WORKDIR)), Some(PROJECT), " (2m timeout)"
        ; "the session's own directory goes unsaid"
    )]
    #[test_case(
        PROJECT, None, Some(PROJECT), " (2m timeout)"
        ; "the session's directory spelled absolute goes unsaid while running"
    )]
    #[test_case(
        "/project/crates/core", None, Some(PROJECT), SUBDIR_ANNOTATION
        ; "a running card spells its directory the way the result will"
    )]
    #[test_case(
        "link", Some(settled_in("target")), Some(PROJECT), " (2m timeout · target/)"
        ; "the result names where a link led"
    )]
    #[test_case(
        "/srv/elsewhere", None, Some(PROJECT), " (2m timeout · /srv/elsewhere/)"
        ; "a directory outside the session reads absolute"
    )]
    #[test_case(
        SUBDIR, None, None, " (2m timeout)"
        ; "nothing to resolve against names nothing"
    )]
    fn a_command_card_names_where_it_runs(
        workdir: &str,
        output: Option<ToolOutput>,
        cwd: Option<&str>,
        expected: &str,
    ) {
        let tl = workdir_card(
            workdir,
            output,
            &in_cwd(test_rctx(UNBROKEN), cwd),
            Some(Disclosure::default()),
        );
        let copied = tl.search_text.lines().next().unwrap_or_default();
        let drawn = lines_text(&tl);

        assert!(copied.ends_with(expected), "{WORKDIR_MSG}: {copied:?}");
        assert!(drawn.contains(expected), "{WORKDIR_MSG}: {drawn:?}");
    }

    /// Only the shell starts somewhere, so a directory on any other call is
    /// not one it runs in, however well it would resolve.
    #[test]
    fn a_tool_that_takes_no_directory_names_none() {
        let msg = deadline_msg(
            PYTHON_EXECUTION_TOOL_NAME,
            Some(serde_json::json!({ "code": DEADLINE_CODE, "workdir": SUBDIR })),
        );
        let drawn = lines_text(&build_tool_lines(
            &msg,
            ToolStatus::InProgress,
            &in_cwd(test_rctx(UNBROKEN), Some(PROJECT)),
            Some(Disclosure::default()),
        ));

        assert!(!drawn.contains(SUBDIR_SHOWN), "{WORKDIR_MSG}: {drawn:?}");
    }

    #[test]
    fn only_the_directory_is_set_in_italics() {
        let tl = workdir_card(
            SUBDIR,
            Some(settled_in(SUBDIR)),
            &in_cwd(test_rctx(UNBROKEN), Some(PROJECT)),
            Some(Disclosure::default()),
        );
        let leans = |text: &str| {
            tl.lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.contains(text))
                .map(|span| span.style.add_modifier.contains(Modifier::ITALIC))
        };

        assert_eq!(leans(SUBDIR_SHOWN), Some(true), "{WORKDIR_ITALIC_MSG}");
        assert_eq!(leans(TIMEOUT_WORD), Some(false), "{WORKDIR_ITALIC_MSG}");
    }

    /// The fold key keeps the brackets from naming what the annotation already
    /// does, and a width that has to break the row still names it whole.
    #[test_case(UNBROKEN ; "with room to spare")]
    #[test_case(SETTLED_TALLY_WIDTH ; "and with none")]
    fn a_closed_command_row_names_its_directory_once(width: u16) {
        let tl = workdir_card(
            SUBDIR,
            Some(settled_in(SUBDIR)),
            &in_cwd(compact_rctx(width), Some(PROJECT)),
            None,
        );
        let text = lines_text(&tl);

        assert!(text.contains(SUBDIR_SHOWN), "{WORKDIR_ONCE_MSG}: {text:?}");
        assert!(
            !text.contains(WORKDIR_BRACKET),
            "{WORKDIR_ONCE_MSG}: {text:?}"
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
