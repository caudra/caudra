use super::{DisplayMessage, ToolProgress, ToolStatus};

use super::code_view;
use crate::animation::{spinner_frame, spinner_str};
use crate::theme;
use caudra_config::{ClockFormat, ToolOutputLines};
use code_view::SectionFlags;
use code_view::{BatchViewMap, BatchViews, RenderLimits, RowTarget};

use std::borrow::Cow;
use std::fmt::Write;
use std::sync::Arc;
use std::time::Instant;

use unicode_width::UnicodeWidthStr;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::markdown::{
    LinkMap, should_truncate, text_to_painted, truncate_output, truncate_output_tail,
    truncation_notice,
};
use caudra_agent::{
    BufferSnapshot, InstructionBlock, ShellOutput, SnapshotSpan, SpanStyle, SubagentProgress,
    ToolInput, ToolOutput,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render_worker::RenderWorker;

pub struct RenderCtx<'a> {
    pub started_at: Instant,
    pub width: u16,
    pub tool_output_lines: &'a ToolOutputLines,
    pub compact: bool,
    /// How much of each batch child the reader has asked to see, by parent
    /// tool id. Looked up here rather than passed in, so every path that
    /// builds a card reads the same views the click that set them named.
    pub batch_views: &'a BatchViewMap,
}

impl RenderCtx<'_> {
    fn views_for(&self, tool_id: Option<&str>) -> BatchViews {
        tool_id
            .and_then(|id| self.batch_views.get(id))
            .cloned()
            .unwrap_or_default()
    }
}

pub const TOOL_INDICATOR: &str = "● ";
pub const TOOL_BODY_INDENT: &str = "  ";
pub(crate) const SPINNER_STYLE_NAME: &str = "spinner";
pub(crate) const SPINNER_STYLE_PREFIX: &str = "spinner:";

const CODE_OUTPUT_DIVIDER: &str = "  ────────────";
const ACTIVITY_PREFIX: &str = "  ├ ";
const ACTIVITY_SEPARATOR: &str = " · ";
pub const RAW_AFFORDANCE: &str = "click for raw";
pub const FILTERED_AFFORDANCE: &str = "click for filtered";
const COMPACT_LOAD_PREFIX: &str = "↳ Loaded ";
const COMPACT_FALLBACK_SIGIL: char = '⚙';
const COMPACT_ARG_LIMIT: usize = 3;
const EDIT_KEYS: &[&str] = &["file_path", "old_string", "new_string"];
/// The tools whose header is built from a pattern the model wrote.
const QUERY_TOOLS: &[&str] = &["file_grep", "file_glob"];
const QUERY_KEY: &str = "pattern";

/// How a tool names itself on a compact row. The `name> ` prefix is gone
/// there, so `label` is what identifies the call, and `header_keys` are the
/// inputs already folded into the header text so the `[k=v]` suffix can skip
/// them. Tools missing from the table fall back to their registered name.
struct CompactTool {
    sigil: char,
    label: &'static str,
    header_keys: &'static [&'static str],
}

/// Header keys are matched ignoring case and underscores, so one spelling
/// covers Workcell's camelCase wire names and the snake_case the legacy tools
/// still carry in restored sessions.
const COMPACT_TOOLS: &[(&str, CompactTool)] = &[
    tool_row("file_read", '→', "Read", &["file_path"]),
    tool_row("file_glob", '✱', "Glob", &["pattern", "path"]),
    tool_row("file_grep", '✱', "Grep", &["pattern", "path"]),
    tool_row("file_write", '←', "Write", &["file_path", "content"]),
    tool_row("file_edit", '←', "Edit", EDIT_KEYS),
    tool_row("file_apply_patch", '%', "Patch", &["patch_text"]),
    tool_row("index", '→', "Index", &["path"]),
    tool_row("websearch", '◈', "Search", &["query"]),
    tool_row("webfetch", '%', "Fetch", &["url"]),
    tool_row("shell", '$', "Shell", &["command"]),
    tool_row("code_execution", '$', "Code", &["code"]),
    tool_row("execution_environment", '⚙', "Env", &[]),
    tool_row("task", '#', "Task", &["prompt", "description"]),
    tool_row("batch", '#', "Batch", &["invocations"]),
    tool_row("todo_write", '⚙', "Todo", &["todos"]),
    tool_row("skill", '→', "Skill", &["name"]),
    tool_row("question", '→', "Ask", &["questions"]),
    tool_row("memory", '⚙', "Memory", &[]),
    tool_row("sessions", '⚙', "Sessions", &[]),
    tool_row("view_image", '→', "Image", &["path"]),
];

const fn tool_row(
    tool: &'static str,
    sigil: char,
    label: &'static str,
    header_keys: &'static [&'static str],
) -> (&'static str, CompactTool) {
    (
        tool,
        CompactTool {
            sigil,
            label,
            header_keys,
        },
    )
}

fn compact_tool(name: &str) -> Option<&'static CompactTool> {
    COMPACT_TOOLS
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, entry)| entry)
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
    let query = QUERY_TOOLS
        .contains(&tool)
        .then(|| raw_input?.get(QUERY_KEY)?.as_str())
        .flatten()
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
    raw_input: Option<&serde_json::Value>,
) -> Option<String> {
    compact_args(
        raw_input,
        compact_tool(tool).map_or(&[], |entry| entry.header_keys),
    )
}

/// The primitive inputs a compact header does not already show, rendered the
/// way opencode does: `[offset=1, limit=260]`.
fn compact_args(raw_input: Option<&serde_json::Value>, header_keys: &[&str]) -> Option<String> {
    let fields = raw_input?.as_object()?;
    let mut rendered = String::new();
    let mut shown = 0;
    for (key, value) in fields
        .iter()
        .filter(|(key, _)| !header_keys.iter().any(|folded| same_key(folded, key)))
    {
        let scalar = match value {
            serde_json::Value::String(text) => Cow::Borrowed(text.as_str()),
            serde_json::Value::Number(number) => Cow::Owned(number.to_string()),
            serde_json::Value::Bool(flag) => Cow::Owned(flag.to_string()),
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
    pub shell_toggle_line: Option<usize>,
    pub content_indent: &'static str,
    pub truncation: SectionFlags,
    /// What each line belongs to, parallel to `lines`, so a splice keeps the
    /// two in step and the async highlight cannot lose a click target.
    pub rows: Vec<Option<RowTarget>>,
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
            | ToolOutput::Instructions { .. } => Some(o),
            ToolOutput::Plain(_)
            | ToolOutput::Markdown(_)
            | ToolOutput::ReadDir(_)
            | ToolOutput::TodoList(_)
            | ToolOutput::Answers(_)
            | ToolOutput::Shell(_)
            | ToolOutput::Image { .. } => None,
            // Children carry their own code and diffs, so a batch reaches the
            // highlighting worker exactly as a lone child would.
            ToolOutput::Batch { ref entries, .. } => (!entries.is_empty()).then_some(o),
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

struct ResolvedOutput<'a> {
    text: Option<Cow<'a, str>>,
    full_text: Option<Cow<'a, str>>,
    skipped: usize,
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

    let expanded = limits.is_output_expanded();
    let (raw_text, already_truncated): (Option<Cow<'a, str>>, usize) = if expanded {
        match &full_text {
            Some(t) => (Some(t.clone()), 0),
            None if output.is_some() => {
                return ResolvedOutput {
                    text: None,
                    full_text: None,
                    skipped: 0,
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
                };
            }
            (None, None) => (None, 0),
        }
    };

    let keep_tail = matches!(output, Some(ToolOutput::Shell(_)));
    let (text, skipped) = match raw_text {
        Some(t) if !t.is_empty() => {
            let tr = if keep_tail {
                truncate_output_tail(&t, limits.output)
            } else {
                truncate_output(&t, limits.output)
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
    }
}

struct ToolLineBuilder {
    lines: Vec<Line<'static>>,
    link_rows: Vec<(usize, Vec<Option<Arc<str>>>)>,
    search_text: String,
    spinner_lines: Vec<(usize, usize)>,
    snapshot_base: Option<usize>,
    shell_toggle_line: Option<usize>,
    content_range: (usize, usize),
    rows: Vec<Option<RowTarget>>,
    width: u16,
    truncation: SectionFlags,
    limits: RenderLimits,
    markdown: bool,
    indicator: Indicator,
}

impl ToolLineBuilder {
    fn new(
        width: u16,
        expanded: SectionFlags,
        max_output_lines: usize,
        indicator: Indicator,
        views: BatchViews,
        budgets: ToolOutputLines,
    ) -> Self {
        let limits = RenderLimits::new(expanded, max_output_lines, views, budgets);
        Self {
            lines: Vec::new(),
            link_rows: Vec::new(),
            search_text: String::new(),
            spinner_lines: Vec::new(),
            snapshot_base: None,
            shell_toggle_line: None,
            content_range: (0, 0),
            rows: Vec::new(),
            width,
            truncation: SectionFlags::default(),
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
        let mut spans = vec![Span::styled(
            format!("{tool_name}> "),
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
        let mut copy = format!("{tool_name}> {header}");
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

    /// The one-line form: a sigil in place of the status dot, a short label
    /// in place of `name> `, and the inputs the header omits.
    fn push_compact_header(
        &mut self,
        tool_name: &str,
        header: &str,
        annotation: Option<&str>,
        raw_input: Option<&serde_json::Value>,
    ) {
        let entry = compact_tool(tool_name);
        let label = entry.map_or(tool_name, |entry| entry.label);

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
        if let Some(args) = compact_args(raw_input, entry.map_or(&[], |entry| entry.header_keys)) {
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

    /// Compact rows carry the sigil where an expanded row carries `● `, so a
    /// finished call still reports success or failure by color.
    fn prepend_compact_sigil(&mut self, tool_name: &str, started_at: Instant) {
        if self.lines.is_empty() {
            return;
        }
        let (text, style) = match self.indicator {
            Indicator::InProgress => (
                format!("{} ", spinner_frame(started_at.elapsed().as_millis())),
                theme::current().spinner,
            ),
            finished => {
                let sigil =
                    compact_tool(tool_name).map_or(COMPACT_FALLBACK_SIGIL, |entry| entry.sigil);
                (format!("{sigil} "), finished_style(finished))
            }
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

    fn prepend_indicator(&mut self, started_at: Instant) {
        if self.lines.is_empty() {
            return;
        }
        let (text, style) = match self.indicator {
            Indicator::InProgress => {
                let ch = spinner_frame(started_at.elapsed().as_millis());
                (format!("{ch} "), theme::current().spinner)
            }
            finished => (TOOL_INDICATOR.into(), finished_style(finished)),
        };
        for (line, span) in &mut self.spinner_lines {
            if *line == 0 {
                *span += 1;
            }
        }
        if matches!(self.indicator, Indicator::InProgress) {
            self.spinner_lines.push((0, 0));
        }
        self.lines[0].spans.insert(0, Span::styled(text, style));
    }

    fn is_in_progress(&self) -> bool {
        matches!(self.indicator, Indicator::InProgress)
    }

    /// A running call reports what it is doing and how far it has got; a
    /// finished one is described by its output, so only the tally survives.
    fn progress_spans(&self, progress: &ToolProgress, out: &mut Vec<Span<'static>>) {
        let theme = theme::current();
        if self.is_in_progress() {
            out.push(Span::styled(
                progress.report.activity.label().to_owned(),
                theme.tool_prefix,
            ));
            if let Some(detail) = progress.report.activity.detail() {
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
        let content = code_view::render_tool_content(input, output, false, self.limits.clone());
        self.truncation.script |= content.truncation.script;
        self.truncation.output |= content.truncation.output;
        let start = self.lines.len();
        for mut line in content.lines {
            line.spans.insert(0, Span::raw(TOOL_BODY_INDENT));
            self.lines.push(line);
        }
        self.content_range = (start, self.lines.len());
        self.rows.resize(start, None);
        self.rows.extend(content.rows);
        if let Some(ToolInput::Code { code, .. } | ToolInput::Script { code, .. }) = input {
            self.push_search_text(code.trim_end());
        }
        if let Some(text) = output.and_then(|o| o.structured_display_text()) {
            self.push_search_text(&text);
        }
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
            if self.markdown {
                self.push_markdown_body(text);
            } else {
                push_text_lines(&mut self.lines, text, TOOL_BODY_INDENT);
            }
            if let Some(full) = &resolved.full_text {
                self.push_search_text(full);
            } else {
                self.push_search_text(text);
            }
            self.push_truncation_count(resolved.skipped);
        }
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

    fn push_markdown_body(&mut self, text: &str) {
        let style = theme::current().assistant;
        let indent = TOOL_BODY_INDENT.len() as u16;
        let (painted, _) = text_to_painted(
            text,
            "",
            style,
            style,
            self.width.saturating_sub(indent),
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
            self.truncation.output = true;
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
        let base = self.lines.len();
        self.snapshot_base = Some(base);
        let total = snapshot.lines.len();
        let frame = spinner_str(started_at.elapsed().as_millis());
        let (lines, spinners) =
            snapshot_to_lines_range(snapshot, TOOL_BODY_INDENT, 0..total, frame, self.indicator);
        self.lines.extend(lines);
        self.spinner_lines
            .extend(spinners.into_iter().map(|(line, span)| (base + line, span)));
        self.push_search_text(&snapshot.text());
        if let Some(text) = search_fallback {
            self.push_search_text(text);
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
        ToolLines {
            lines: self.lines,
            links,
            search_text: self.search_text,
            highlight,
            spinner_lines: self.spinner_lines,
            snapshot_base: self.snapshot_base,
            shell_toggle_line: self.shell_toggle_line,
            content_indent,
            truncation: self.truncation,
            rows,
        }
    }
}

fn push_text_lines(lines: &mut Vec<Line<'static>>, text: &str, indent: &'static str) {
    let style = theme::current().tool;
    for line in text.lines() {
        lines.push(Line::from(vec![
            Span::styled(indent, style),
            Span::styled(line.to_owned(), style),
        ]));
    }
}

/// Bakes snapshot spans onto `out`. `"spinner"`-styled spans bake to the
/// current frame while a tool is in progress, and `on_spinner` gets their
/// span index in the same pass, so animation offsets can never drift from
/// the baked spans. Finished tools bake a static dot instead and record no
/// spinner position, so a stale `"spinner"` span can never keep animating.
fn bake_spans(
    src: &[SnapshotSpan],
    out: &mut Vec<Span<'static>>,
    spinner_frame: &'static str,
    indicator: Indicator,
    mut on_spinner: impl FnMut(usize),
) {
    for span in src {
        if matches!(&span.style, SpanStyle::Named(n) if n == SPINNER_STYLE_NAME) {
            match indicator {
                Indicator::InProgress => {
                    on_spinner(out.len());
                    out.push(Span::styled(spinner_frame, theme::current().spinner));
                }
                finished => out.push(Span::styled(TOOL_INDICATOR, finished_style(finished))),
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
    if matches!(indicator, Indicator::Error) {
        theme::current().tool_error
    } else {
        theme::current().tool_success
    }
}

fn snapshot_to_lines_range(
    snapshot: &BufferSnapshot,
    indent: &str,
    range: std::ops::Range<usize>,
    spinner_frame: &'static str,
    indicator: Indicator,
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

/// `expansion` is `None` on a compact row the reader has not opened, which is
/// the only state that draws a header with no body.
pub fn build_tool_lines(
    msg: &DisplayMessage,
    status: ToolStatus,
    rctx: &RenderCtx,
    expansion: Option<SectionFlags>,
) -> ToolLines {
    let tool_name = msg.role.tool_name().unwrap_or("?");
    let (header, body) = match msg.text.split_once('\n') {
        Some((h, b)) => (h, Some(b)),
        None => (msg.text.as_str(), None),
    };
    let expanded = expansion.unwrap_or_default();

    let mut b = ToolLineBuilder::new(
        rctx.width,
        expanded,
        rctx.tool_output_lines.get(tool_name),
        status.into(),
        rctx.views_for(msg.role.tool_id()),
        *rctx.tool_output_lines,
    );
    b.apply_output_format(msg.tool_output.as_deref());
    if rctx.compact {
        b.push_compact_header(
            tool_name,
            header,
            msg.annotation.as_deref(),
            msg.tool_raw_input.as_deref(),
        );
        b.prepend_compact_sigil(tool_name, rctx.started_at);
    } else {
        b.push_header(
            tool_name,
            header,
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
        b.truncation.output = msg.render_snapshot.is_some()
            || msg.tool_input.is_some()
            || msg.tool_output.is_some()
            || body.is_some_and(|body| !body.trim().is_empty());
        return b.finish(
            msg.tool_input.clone(),
            msg.tool_output.clone(),
            TOOL_BODY_INDENT,
        );
    }
    let has_snapshot = msg.render_snapshot.is_some();
    b.push_code_content(
        msg.tool_input.as_deref(),
        if has_snapshot {
            None
        } else {
            msg.tool_output.as_deref()
        },
    );
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

    let exp = SectionFlags {
        script: false,
        output: expanded,
        shell_raw: false,
    };
    let mut b = ToolLineBuilder::new(
        width,
        exp,
        code_view::instruction_limit(expanded),
        Indicator::Success,
        BatchViews::default(),
        // A loaded instruction card has no children to look themselves up.
        ToolOutputLines::default(),
    );
    b.push_header("load", header, annotation.as_deref(), None, None, None);
    b.prepend_indicator(Instant::now());

    let start = b.lines.len();
    let has_truncation =
        code_view::render_instructions(blocks, &mut b.lines, b.limits.output, false);
    b.truncation.output |= has_truncation;
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
        shell_toggle_line: None,
        content_indent: TOOL_BODY_INDENT,
        rows: Vec::new(),
        truncation: SectionFlags {
            script: false,
            output: true,
            shell_raw: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: ToolOutputLines = ToolOutputLines::DEFAULT;
    use crate::components::{DisplayRole, ToolRole};
    use crate::markdown::TRUNCATION_PREFIX;
    use caudra_agent::tools::{FILE_READ_TOOL_NAME, SHELL_TOOL_NAME, TASK_TOOL_NAME, ToolEffect};
    use caudra_agent::{
        ShellFilterInfo, SnapshotLine, SnapshotSpan, SubagentActivity, TextOutput, ToolInput,
        ToolOutput,
    };
    use std::time::Duration;
    use test_case::test_case;

    static NO_VIEWS: std::sync::LazyLock<BatchViewMap> =
        std::sync::LazyLock::new(BatchViewMap::new);

    fn test_rctx(width: u16) -> RenderCtx<'static> {
        RenderCtx {
            started_at: Instant::now(),
            width,
            tool_output_lines: &TOL,
            compact: false,
            batch_views: &NO_VIEWS,
        }
    }

    fn compact_rctx(width: u16) -> RenderCtx<'static> {
        RenderCtx {
            compact: true,
            ..test_rctx(width)
        }
    }

    fn exp(both: bool) -> SectionFlags {
        SectionFlags {
            script: both,
            output: both,
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
            live_output: None,
            annotation: None,
            progress: None,
            plan_path: None,
            truncated_lines: 0,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            reasoning_open: None,
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
        );
        let filtered_expanded = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(SectionFlags {
                output: true,
                ..SectionFlags::default()
            }),
        );
        let raw_collapsed = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(SectionFlags {
                shell_raw: true,
                ..SectionFlags::default()
            }),
        );
        let raw_expanded = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(SectionFlags {
                output: true,
                shell_raw: true,
                ..SectionFlags::default()
            }),
        );
        let collapsed_text = lines_text(&collapsed);

        assert!(collapsed_text.contains("model_8"));
        assert!(!collapsed_text.contains("model_1"));
        assert!(!collapsed_text.contains("raw_8"));
        assert!(
            collapsed_text.contains("filtered · cargo, progress · 80% smaller · click for raw")
        );
        assert!(collapsed.truncation.output);
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
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
        let tl = build_tool_lines(&msg, status, &test_rctx(80), Some(SectionFlags::default()));
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
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
            live_output: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            reasoning_open: None,
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
        );
        assert_hr_fits(&tl, width);
    }

    fn index_msg(body: &str) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Success,
                name: "index".into(),
            })),
            text: format!("src/lib.rs\n{body}"),
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::Plain(body.to_owned().into()))),
            live_output: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            reasoning_open: None,
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
        );
        let expanded = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(SectionFlags {
                script: false,
                output: true,
                shell_raw: false,
            }),
        );

        assert!(collapsed.truncation.output);
        assert!(lines_text(&collapsed).contains("item_1"));
        assert!(!lines_text(&collapsed).contains("item_8"));
        assert!(lines_text(&expanded).contains("item_8"));
        assert!(
            collapsed.lines[0].spans.iter().any(
                |span| span.content == "src/lib.rs" && span.style == theme::current().tool_path
            )
        );
    }

    fn snapshot_msg(snapshot: BufferSnapshot) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Success,
                name: "index".into(),
            })),
            text: "src/lib.rs\nplain fallback".into(),
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::Plain("plain fallback".into()))),
            live_output: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
            snapshot_theme_gen: 0,
            reasoning_open: None,
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
        );
        // indicator + `tool> ` prefix + "3 tools " sit before the header spinner.
        assert_eq!(tl.spinner_lines, vec![(0, 3), (0, 0)]);
    }

    const DENIAL_MSG: &str = "Permission denied: user rejected";

    fn error_snapshot_msg(snapshot_lines: &[&str], output: &str) -> DisplayMessage {
        DisplayMessage {
            role: DisplayRole::Tool(Box::new(ToolRole {
                id: "t1".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Error,
                name: "code_execution".into(),
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
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
        let limits = RenderLimits::new(
            SectionFlags::default(),
            TOL.get(tool),
            BatchViews::default(),
            TOL,
        );
        let resolved = resolve_output(output.as_ref(), body, None, 0, limits, false);
        assert_eq!(resolved.text.is_some(), expect_text);
    }

    #[test]
    fn resolve_output_pre_truncated_forwarded() {
        let limits = RenderLimits::new(
            SectionFlags::default(),
            TOL.get("bash"),
            BatchViews::default(),
            TOL,
        );
        let resolved = resolve_output(None, Some("short"), None, 42, limits, false);
        assert_eq!(resolved.skipped, 42);
    }

    #[test]
    fn resolve_output_truncation_overrides_pre_truncated() {
        let long = n_lines(200);
        let limits = RenderLimits::new(
            SectionFlags::default(),
            TOL.get("bash"),
            BatchViews::default(),
            TOL,
        );
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
            annotation: None,
            progress: None,
            plan_path: None,
            truncated_lines,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            reasoning_open: None,
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
        assert!(collapsed.truncation.any());
        assert!(!expanded.truncation.any());
        assert!(expanded_text.contains("line 0"));
        assert!(expanded_text.contains("line 199"));
        assert!(collapsed_text.contains("line 0"));
        assert!(!collapsed_text.contains("line 199"));
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
        assert_eq!(tl.truncation.any(), expect_truncation);
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
        assert_eq!(tl.truncation.any(), expect_truncation);
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
            live_output: None,
            annotation: None,
            progress: None,
            plan_path: None,
            truncated_lines: 0,
            timestamp: None,
            turn_usage: None,
            render_snapshot: None,
            render_header: None,
            snapshot_theme_gen: 0,
            reasoning_open: None,
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
        assert_eq!(tl.truncation.any(), expect_truncation);
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
            Some(SectionFlags::default()),
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
            collapsed.truncation.output,
            "a hidden snapshot has to leave a click target behind"
        );
        assert!(!lines_text(&collapsed).contains("rendered by lua"));

        let opened = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &compact_rctx(80),
            Some(SectionFlags::default()),
        );
        assert!(lines_text(&opened).contains("rendered by lua"));
    }

    #[test]
    fn a_bodyless_compact_row_reports_nothing_to_open() {
        let msg = bash_msg("ls", ToolStatus::Success, None, None);
        let tl = build_tool_lines(&msg, ToolStatus::Success, &compact_rctx(80), None);

        assert_eq!(tl.lines.len(), 1);
        assert!(!tl.truncation.any());
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
    #[test_case(Some(SectionFlags::default()) ; "expanded")]
    fn a_running_subagent_reports_its_tool_under_the_header(expansion: Option<SectionFlags>) {
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
            "  ├ shell cargo nextest run · 3 tools · 1m 3.4s"
        );
    }

    /// What it was doing is stale the moment it stops; what it did is not.
    #[test_case(ToolStatus::Success ; "success")]
    #[test_case(ToolStatus::Error   ; "error")]
    fn a_settled_subagent_keeps_only_its_tally(status: ToolStatus) {
        let msg = subagent_msg(status, Some(running_tool_report(7)));

        let tl = build_tool_lines(&msg, status, &test_rctx(80), Some(SectionFlags::default()));

        let text = lines_text(&tl);
        assert!(text.contains("├ 7 tools · 1m 3.4s"), "{text}");
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
            Some(SectionFlags::default()),
        );

        assert!(
            lines_text(&tl).contains(&format!("├ {expected}")),
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
            Some(SectionFlags::default()),
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
            text.contains(" · shell cargo nextest run · 3 tools · 1m 3.4s"),
            "{text}"
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
            Some(SectionFlags::default()),
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
            Some(SectionFlags::default()),
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
                name: "index".into(),
            })),
            text: "src/lib.rs\nbody_text_here".into(),
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: Some(Arc::new(ToolOutput::Plain("llm_output_here".into()))),
            live_output: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
            snapshot_theme_gen: 0,
            reasoning_open: None,
            thinking_duration: None,
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(SectionFlags::default()),
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
                name: "index".into(),
            })),
            text: "header\nbody_fallback".into(),
            source: None,
            tool_input: None,
            tool_raw_input: None,
            tool_output: None,
            live_output: None,
            annotation: None,
            progress: None,
            plan_path: None,
            timestamp: None,
            turn_usage: None,
            truncated_lines: 0,
            render_snapshot: Some(snapshot),
            render_header: None,
            snapshot_theme_gen: 0,
            reasoning_open: None,
            thinking_duration: None,
        };
        let tl = build_tool_lines(
            &msg,
            ToolStatus::Success,
            &test_rctx(80),
            Some(SectionFlags::default()),
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
        let (lines, _) =
            snapshot_to_lines_range(&snapshot, ">>", 0..1, "⠋ ", Indicator::InProgress);
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
        let (lines, _) = snapshot_to_lines_range(&snapshot, "", 0..1, "⠋ ", Indicator::InProgress);
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
        let (lines, spinners) =
            snapshot_to_lines_range(&snapshot, "", 0..2, "⠹ ", Indicator::InProgress);
        assert_eq!(spinners, vec![(1, 2)]);
        assert_eq!(lines[1].spans[2].content.as_ref(), "⠹ ");
    }

    #[test_case(ToolStatus::Success ; "success_bakes_dot")]
    #[test_case(ToolStatus::Error ; "error_bakes_dot")]
    fn done_snapshot_with_spinner_span_bakes_dot_and_records_no_spinner(status: ToolStatus) {
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
            Some(SectionFlags::default()),
        );
        assert_eq!(tl.spinner_lines, vec![]);
        let body = tl.lines.get(1).expect("snapshot body line");
        let dot = body.spans.last().expect("baked dot span");
        assert_eq!(dot.content.as_ref(), TOOL_INDICATOR);
    }

    #[test]
    fn done_header_with_spinner_span_bakes_dot_and_records_no_spinner() {
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
            Some(SectionFlags::default()),
        );
        assert_eq!(tl.spinner_lines, vec![]);
        let header_line = tl.lines.first().expect("header line");
        let dot = header_line.spans.last().expect("baked dot span");
        assert_eq!(dot.content.as_ref(), TOOL_INDICATOR);
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
}
