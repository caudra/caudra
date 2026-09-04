use std::any::Any;
use std::fmt::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use caudra_providers::{AgentError, ContentBlock, Message, Role, StopReason, TokenUsage};
use caudra_storage::tool_outputs::ToolOutputRef;
use flume::Sender;
use serde::{Deserialize, Serialize};
use strum::Display;

use crate::agent::{GoalResult, GoalVerdict};
use crate::permissions::PermissionRequest;
use crate::tools::ToolEffect;

pub const NO_FILES_FOUND: &str = "No files found";
pub const INDEX_TRUNCATED: &str = "[truncated]";

const SECONDS_PER_MINUTE: u64 = 60;
const TALLY_SEPARATOR: &str = " · ";
const THOUGHT_TITLE_FENCE: &str = "**";
const PARAGRAPH_BREAK: &str = "\n\n";
const CRLF_PARAGRAPH_BREAK: &str = "\r\n\r\n";
const THINKING_LABEL: &str = "thinking";
const RESPONDING_LABEL: &str = "responding";
const COMPACTING_LABEL: &str = "compacting";
const RETRYING_LABEL: &str = "retrying";
const AWAITING_PERMISSION_LABEL: &str = "awaiting permission";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepFileEntry {
    pub path: String,
    pub groups: Vec<GrepMatchGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatchGroup {
    pub lines: Vec<GrepLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepLine {
    pub line_nr: usize,
    pub text: String,
    pub is_match: bool,
}

impl GrepLine {
    pub fn matched(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            line_nr,
            text: text.into(),
            is_match: true,
        }
    }

    pub fn context(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            line_nr,
            text: text.into(),
            is_match: false,
        }
    }
}

impl GrepMatchGroup {
    pub fn single(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            lines: vec![GrepLine::matched(line_nr, text)],
        }
    }

    pub fn match_count(&self) -> usize {
        self.lines.iter().filter(|l| l.is_match).count()
    }
}

impl GrepFileEntry {
    pub fn match_count(&self) -> usize {
        self.groups.iter().map(|g| g.match_count()).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    #[serde(default)]
    pub priority: TodoPriority,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    pub fn marker(self) -> &'static str {
        match self {
            Self::Completed => "[✓]",
            Self::InProgress => "[•]",
            Self::Pending => "[ ]",
            Self::Cancelled => "[x]",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TodoPriority {
    High,
    #[default]
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolInput {
    Code {
        language: String,
        code: String,
    },
    /// Nothing produces this anymore (script rendering moved to Lua), but
    /// old persisted sessions still contain it and must keep loading.
    Script {
        language: String,
        code: String,
    },
}

/// A question put to the user, as the model asked it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskedQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// What the user picked for one question. `labels` is empty when the question
/// was skipped, and may hold text the user typed rather than an offered label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub header: String,
    pub labels: Vec<String>,
}

/// The run has parked on a question. The front end that shows the form answers
/// through the user-response channel, which the asking tool holds locked for
/// the duration, so no id is needed to route the reply back.
#[derive(Debug, Clone, Serialize)]
pub struct QuestionEvent {
    pub questions: Vec<AskedQuestion>,
}

/// One child of a batch, as the reader sees it. `output` is the child's own
/// structured result, so a batch renders each child exactly as the same tool
/// renders standalone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchToolEntry {
    pub tool: String,
    /// The child's header line, from the same summary the transcript shows.
    pub summary: String,
    pub status: BatchToolStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<ToolInput>,
    /// What the child was called with, for the arguments its header leaves
    /// out. The same source a standalone row reads, so a child in a batch
    /// names its inputs the way it would on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ToolOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
}

/// One child of a running batch changing state. Only the child that moved is
/// sent: the roster arrived with the batch's `ToolStart`, so the reader
/// already knows what it is patching.
#[derive(Debug, Clone, Serialize)]
pub struct BatchProgressEvent {
    /// The batch's own tool-use id.
    pub id: String,
    pub index: usize,
    pub entry: BatchToolEntry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BatchToolStatus {
    Pending,
    Running,
    Success,
    Error,
}

impl BatchToolStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Success | Self::Error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionBlock {
    pub path: String,
    pub content: String,
}

fn append_instructions(out: &mut String, blocks: &[InstructionBlock]) {
    for block in blocks {
        out.push_str("\n\n---\nInstructions from: ");
        out.push_str(&block.path);
        out.push('\n');
        out.push_str(&block.content);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextOutput {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Vec<InstructionBlock>>,
    /// Structured plugin state saved with the session, so `restore` never
    /// has to re-parse its own llm output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lua_provenance: Option<LuaToolProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellFilterInfo {
    /// Reductions that ran, in order. A corpus rule appears under its own name;
    /// the command-independent progress collapse appears as `progress`.
    pub stages: Vec<String>,
    pub unfiltered_utf8_bytes: usize,
    pub filtered_utf8_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellOutput {
    pub model_text: String,
    pub relative_workdir: String,
    pub timeout_ms: u64,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub output_limit_exceeded: bool,
    pub final_sequence: u64,
    pub stdout_utf8_bytes: u64,
    pub stderr_utf8_bytes: u64,
    pub stdout: String,
    pub stderr: String,
    pub stdout_capture_truncated: bool,
    pub stderr_capture_truncated: bool,
    pub stdout_preview_truncated: bool,
    pub stderr_preview_truncated: bool,
    /// Redraw frames absorbed while rendering the capture as a terminal would
    /// show it. `stdout_utf8_bytes` still reports what the command wrote.
    pub stdout_redraws_collapsed: u64,
    pub stderr_redraws_collapsed: u64,
    pub filter: Option<ShellFilterInfo>,
}

impl ShellOutput {
    /// Frames a reader is not being shown, across both streams.
    pub fn redraws_collapsed(&self) -> u64 {
        self.stdout_redraws_collapsed
            .saturating_add(self.stderr_redraws_collapsed)
    }

    pub fn raw_text(&self) -> String {
        match (self.stdout.is_empty(), self.stderr.is_empty()) {
            (false, true) => self.stdout.clone(),
            (true, false) => self.stderr.clone(),
            (false, false) => format!(
                "stdout tail:\n{}\nstderr tail:\n{}",
                self.stdout, self.stderr
            ),
            (true, true) => String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LuaToolProvenance {
    pub plugin: String,
    pub contract: String,
    #[serde(default)]
    pub error_restore_allowed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexLineSemantic {
    Section,
    Item,
    Dimmed,
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSourceRange {
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexLine {
    pub output_line: usize,
    pub text: String,
    pub semantic: IndexLineSemantic,
    pub body: Option<String>,
    pub source_range: Option<IndexSourceRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexDirectoryEntryKind {
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDirectoryEntry {
    pub name: String,
    pub kind: IndexDirectoryEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum IndexOutput {
    File {
        path: String,
        relative_path: String,
        language: String,
        skeleton: String,
        lines: Vec<IndexLine>,
        source_line_count: usize,
        parse_error: bool,
        truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Vec<InstructionBlock>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<serde_json::Value>,
    },
    Directory {
        path: String,
        relative_path: String,
        entries: Vec<IndexDirectoryEntry>,
        total_count: usize,
        truncated: bool,
        listing: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Vec<InstructionBlock>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<serde_json::Value>,
    },
}

impl From<String> for TextOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            instructions: None,
            state: None,
            lua_provenance: None,
        }
    }
}

impl From<&str> for TextOutput {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            instructions: None,
            state: None,
            lua_provenance: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolOutput {
    Plain(TextOutput),
    Markdown(TextOutput),
    ReadCode {
        path: String,
        start_line: usize,
        lines: Vec<String>,
        #[serde(default)]
        total_lines: usize,
        #[serde(default)]
        instructions: Option<Vec<InstructionBlock>>,
    },
    ReadDir(TextOutput),
    Diff {
        path: String,
        before: String,
        after: String,
        summary: String,
    },
    TodoList(Vec<TodoItem>),
    Answers(Vec<Answer>),
    WriteCode {
        path: String,
        byte_count: usize,
        lines: Vec<String>,
    },
    /// A change expressed as unified diffs rather than before/after text.
    /// `Diff` cannot carry it: a patch may touch several files, and the two
    /// sides of each hunk are all that is known about any of them.
    Patch {
        files: Vec<PatchedFile>,
    },

    GrepResult {
        entries: Vec<GrepFileEntry>,
        /// Present when a bound stopped the search rather than the tree ending.
        /// Without it a capped result is indistinguishable from a complete one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capped: Option<SearchCap>,
    },
    Index(IndexOutput),
    Shell(ShellOutput),
    /// `text` is what the model was told; `entries` is the same run written
    /// for a reader. Sessions written while batch was a Lua plugin carry only
    /// `text`, so `entries` defaults to empty and the body falls back to it.
    Batch {
        #[serde(default)]
        entries: Vec<BatchToolEntry>,
        text: String,
    },
    Instructions {
        blocks: Vec<InstructionBlock>,
    },
    Image {
        source: caudra_providers::ImageSource,
        /// Caption for the tool_result block, e.g. "[image: slack.jpeg 222KB]";
        /// the pixels ride separately as a `ContentBlock::Image`.
        text: String,
    },
}

/// How far a search reached before a bound stopped it. Both counts are lower
/// bounds: an exact match total would mean reading every remaining file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchCap {
    pub files_scanned: usize,
    pub files_listed: usize,
}

/// One file's share of a patch. `path` is the project-relative spelling, which
/// is what a reader recognizes and what the diff header already carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchedFile {
    pub path: String,
    /// Unified diff for this file alone.
    pub patch: String,
    pub additions: usize,
    pub deletions: usize,
}

/// Saturating arithmetic so callers can't overflow with any combination of inputs.
fn lines_remaining_after(total: usize, start_line: usize, shown: usize) -> usize {
    let end = start_line.saturating_add(shown).saturating_sub(1);
    total.saturating_sub(end)
}

/// Lines are what the rest of the annotations count, so a write reports them
/// too. Sessions written before the line split only carry the byte count.
fn written_size(byte_count: usize, lines: &[String]) -> String {
    if lines.is_empty() && byte_count > 0 {
        return format!("{byte_count} bytes");
    }
    format!("{} lines", lines.len())
}

impl ToolOutput {
    /// Short header suffix summarizing the output, e.g. `12 lines`.
    /// The UI uses it on tool completion, and `caudra.agent.call_tool` falls
    /// back to it when the tool's reply has no annotation of its own.
    pub fn annotation(&self) -> Option<String> {
        match self {
            Self::ReadCode {
                lines, total_lines, ..
            } => {
                let shown = lines.len();
                if *total_lines > shown {
                    Some(format!("{shown} of {total_lines} lines"))
                } else {
                    Some(format!("{shown} lines"))
                }
            }
            Self::WriteCode {
                byte_count, lines, ..
            } => Some(written_size(*byte_count, lines)),
            Self::Diff { before, after, .. } => Some(crate::diff::stat(before, after)),
            Self::Patch { files } => Some(crate::diff::format_stat(
                files.iter().map(|f| f.additions).sum(),
                files.iter().map(|f| f.deletions).sum(),
            )),
            Self::GrepResult { entries, capped } => {
                let matches: usize = entries.iter().map(|e| e.match_count()).sum();
                let files = entries.len();
                let f = if files == 1 { "file" } else { "files" };
                Some(match capped {
                    Some(cap) => format!(
                        "{matches} matches in {files} {f} (capped, {}/{} searched)",
                        cap.files_scanned, cap.files_listed
                    ),
                    None => format!("{matches} matches in {files} {f}"),
                })
            }
            Self::ReadDir(t) => {
                let n = t.text.lines().count();
                Some(format!("{n} entries"))
            }
            Self::Index(IndexOutput::File { lines, .. }) => Some(format!("{} lines", lines.len())),
            Self::Index(IndexOutput::Directory {
                total_count,
                truncated,
                ..
            }) => Some(if *truncated {
                format!("at least {total_count} entries")
            } else {
                format!("{total_count} entries")
            }),
            Self::Shell(output) => Some(if output.timed_out {
                "timed out".into()
            } else if output.output_limit_exceeded {
                "output limit exceeded".into()
            } else if let Some(exit_code) = output.exit_code {
                format!("exit {exit_code}")
            } else if let Some(signal) = output.signal {
                format!("signal {signal}")
            } else {
                "exit unknown".into()
            }),
            Self::Plain(text) | Self::Markdown(text) if !text.text.is_empty() => {
                let n = text.text.lines().count();
                Some(format!("{n} lines"))
            }
            Self::Image { text, .. } => Some(
                text.strip_prefix("[image: ")
                    .and_then(|t| t.strip_suffix(']'))
                    .unwrap_or(text)
                    .to_string(),
            ),
            _ => None,
        }
    }

    /// Only here for old persisted sessions that still have `WriteCode`/`Diff` variants.
    /// New code should use `ToolDoneEvent::written_path` instead.
    pub fn written_path(&self) -> Option<&str> {
        match self {
            Self::WriteCode { path, .. } | Self::Diff { path, .. } => Some(path),
            _ => None,
        }
    }

    pub fn instructions(&self) -> Option<&[InstructionBlock]> {
        match self {
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => t.instructions.as_deref(),
            Self::ReadCode { instructions, .. } => instructions.as_deref(),
            Self::Index(IndexOutput::File { instructions, .. })
            | Self::Index(IndexOutput::Directory { instructions, .. }) => instructions.as_deref(),
            _ => None,
        }
    }

    pub fn owned_instructions(&self) -> Option<Vec<InstructionBlock>> {
        self.instructions()
            .filter(|b| !b.is_empty())
            .map(|b| b.to_vec())
    }

    pub fn is_markdown(&self) -> bool {
        matches!(self, Self::Markdown(_))
    }

    pub fn state(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => t.state.as_ref(),
            Self::Index(IndexOutput::File { state, .. })
            | Self::Index(IndexOutput::Directory { state, .. }) => state.as_ref(),
            _ => None,
        }
    }

    pub fn lua_provenance(&self) -> Option<&LuaToolProvenance> {
        match self {
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => t.lua_provenance.as_ref(),
            _ => None,
        }
    }

    pub fn set_lua_provenance(&mut self, provenance: LuaToolProvenance) {
        if let Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) = self {
            t.lua_provenance = Some(provenance);
        }
    }

    pub fn structured_display_text(&self) -> Option<String> {
        match self {
            Self::Diff { .. }
            | Self::ReadCode { .. }
            | Self::ReadDir(_)
            | Self::WriteCode { .. }
            | Self::GrepResult { .. }
            | Self::Index(_)
            | Self::Shell(_)
            | Self::TodoList(_)
            | Self::Answers(_) => Some(self.as_display_text()),
            _ => None,
        }
    }

    pub fn is_empty_result(&self) -> bool {
        match self {
            Self::GrepResult { entries, .. } => entries.is_empty(),
            Self::Index(IndexOutput::File { skeleton, .. }) => skeleton.is_empty(),
            Self::Index(IndexOutput::Directory { listing, .. }) => listing.is_empty(),
            Self::Shell(output) => output.stdout.is_empty() && output.stderr.is_empty(),
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => t.text.is_empty(),
            _ => false,
        }
    }

    pub fn as_text(&self) -> String {
        match self {
            Self::Diff { summary, .. } => summary.clone(),
            Self::TodoList(_) => "ok".into(),
            Self::Shell(output) => output.model_text.clone(),
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => {
                let mut out = t.text.clone();
                if let Some(blocks) = &t.instructions {
                    append_instructions(&mut out, blocks);
                }
                out
            }
            Self::ReadCode { instructions, .. } => {
                let mut out = self.as_display_text();
                if let Some(blocks) = instructions {
                    append_instructions(&mut out, blocks);
                }
                out
            }
            Self::Index(IndexOutput::File { instructions, .. })
            | Self::Index(IndexOutput::Directory { instructions, .. }) => {
                let mut out = self.as_display_text();
                if let Some(blocks) = instructions {
                    append_instructions(&mut out, blocks);
                }
                out
            }
            _ => self.as_display_text(),
        }
    }

    pub fn as_display_text(&self) -> String {
        match self {
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => t.text.clone(),
            Self::Index(IndexOutput::File { skeleton, .. }) => skeleton.clone(),
            Self::Index(IndexOutput::Directory { listing, .. }) => listing.clone(),
            Self::Shell(output) => output.raw_text(),
            Self::ReadCode {
                start_line,
                lines,
                total_lines,
                ..
            } => {
                let mut out: String = lines
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{}: {line}", start_line + i))
                    .collect::<Vec<_>>()
                    .join("\n");
                let remaining = lines_remaining_after(*total_lines, *start_line, lines.len());
                if remaining > 0 {
                    out.push_str(&format!(
                        "\n\n...\n\nTruncated lines: {}-{}. Use offset={} to read further.",
                        start_line + lines.len(),
                        total_lines,
                        start_line + lines.len(),
                    ));
                }
                out
            }
            Self::Diff {
                path,
                before,
                after,
                summary,
            } => crate::diff::unified_text(
                before,
                after,
                summary,
                &crate::tools::relative_path(path),
            ),
            Self::Patch { files } => files
                .iter()
                .map(|f| f.patch.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Answers(answers) => answers
                .iter()
                .map(|a| format!("{}: {}", a.header, a.labels.join(", ")))
                .collect::<Vec<_>>()
                .join("\n"),
            Self::TodoList(items) => {
                if items.is_empty() {
                    return "No todos.".into();
                }
                items
                    .iter()
                    .map(|t| format!("{} ({}) {}", t.status.marker(), t.priority, t.content))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Self::WriteCode {
                path,
                byte_count,
                lines,
            } => {
                let display = crate::tools::relative_path(path);
                format!("wrote {} to {display}", written_size(*byte_count, lines))
            }
            Self::GrepResult { entries, .. } => {
                let mut out = String::new();
                for (i, entry) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push('\n');
                    }
                    out.push_str(&entry.path);
                    out.push(':');
                    let has_context = entry.groups.iter().any(|g| g.lines.len() > 1);
                    for (gi, group) in entry.groups.iter().enumerate() {
                        if gi > 0 && has_context {
                            out.push_str("\n  --");
                        }
                        for line in &group.lines {
                            let sep = if line.is_match { ":" } else { " " };
                            let _ = write!(out, "\n  {}{sep} {}", line.line_nr, line.text);
                        }
                    }
                }
                out
            }
            Self::Batch { text, .. } | Self::Image { text, .. } => text.clone(),
            Self::Instructions { blocks } => {
                let mut out = String::new();
                append_instructions(&mut out, blocks);
                out
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStartEvent {
    pub id: String,
    pub tool: Arc<str>,
    /// Stamped where the tool is known, so the transcript never has to ask a
    /// registry that may have changed since the call ran.
    pub effect: ToolEffect,
    pub summary: String,
    pub render_header: Option<BufferSnapshot>,
    pub annotation: Option<String>,
    pub input: Option<ToolInput>,
    pub raw_input: Option<serde_json::Value>,
    pub output: Option<ToolOutput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolOutputLimits {
    pub max_lines: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDoneEvent {
    pub id: String,
    pub tool: Arc<str>,
    pub output: ToolOutput,
    pub is_error: bool,
    pub annotation: Option<String>,
    pub written_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub written_paths: Vec<String>,
    pub output_ref: Option<ToolOutputRef>,
    #[serde(skip)]
    pub output_limits: Option<ToolOutputLimits>,
    #[serde(skip)]
    pub model_suffix: Option<String>,
    #[serde(skip)]
    pub model_output: Option<String>,
    #[serde(skip)]
    pub model_output_from_ref: bool,
}

const UNKNOWN_TOOL: &str = "unknown";
const MODEL_SUFFIX_SEPARATOR: &str = "\n\n";

impl ToolDoneEvent {
    pub fn error(id: String, message: impl Into<String>) -> Self {
        let message: String = message.into();
        Self {
            id,
            tool: Arc::from(UNKNOWN_TOOL),
            output: ToolOutput::Plain(message.into()),
            is_error: true,
            annotation: None,
            written_path: None,
            written_paths: Vec::new(),
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
        }
    }

    pub fn written_path(&self) -> Option<&str> {
        if self.is_error {
            return None;
        }
        self.written_path
            .as_deref()
            .or_else(|| self.written_paths.first().map(String::as_str))
            .or_else(|| self.output.written_path())
    }

    pub fn written_paths(&self) -> impl Iterator<Item = &str> {
        let legacy = self.written_path().filter(|path| {
            !self
                .written_paths
                .iter()
                .any(|candidate| candidate == *path)
        });
        self.written_paths
            .iter()
            .map(String::as_str)
            .chain(legacy)
            .filter(|_| !self.is_error)
    }

    pub fn model_suffix(&self) -> Option<&str> {
        self.model_suffix.as_deref()
    }

    pub fn with_model_suffix(mut self, model_suffix: Option<String>) -> Self {
        self.model_suffix = model_suffix;
        self
    }

    pub(crate) fn composed_model_output(&self) -> String {
        let mut content = self
            .model_output
            .clone()
            .unwrap_or_else(|| self.output.as_text());
        if let Some(model_suffix) = self.model_suffix() {
            append_model_suffix(&mut content, model_suffix);
        }
        content
    }

    pub fn wrote_to(&self, plan_path: &Path) -> bool {
        self.written_paths()
            .any(|written_path| Path::new(written_path) == plan_path)
    }
}

fn append_model_suffix(content: &mut String, model_suffix: &str) {
    let model_suffix = model_suffix.trim_matches(['\r', '\n']);
    if model_suffix.is_empty() {
        return;
    }
    let content_len = content.trim_end_matches(['\r', '\n']).len();
    content.truncate(content_len);
    if !content.is_empty() {
        content.push_str(MODEL_SUFFIX_SEPARATOR);
    }
    content.push_str(model_suffix);
}

pub fn tool_results(results: Vec<ToolDoneEvent>) -> Message {
    let mut content = Vec::with_capacity(results.len());
    let mut images = Vec::new();
    let mut tool_result_image_owners = Vec::new();
    for mut r in results {
        let result_content = r
            .model_output
            .take()
            .unwrap_or_else(|| r.composed_model_output());
        if let ToolOutput::Image { source, .. } = &r.output {
            images.push(ContentBlock::Image {
                source: source.clone(),
            });
            tool_result_image_owners.push(r.id.clone());
        }
        content.push(ContentBlock::ToolResult {
            tool_use_id: r.id,
            content: result_content,
            is_error: r.is_error,
            output_ref: r.output_ref,
        });
    }
    // Anthropic wants every tool_result before other content in the user
    // message, so images go after all results.
    content.extend(images);
    Message {
        role: Role::User,
        content,
        tool_result_image_owners,
        ..Default::default()
    }
}

/// Why a run ended. The provider's `StopReason` describes one turn, this
/// describes the whole run, including the endings only the agent knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Display)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum DoneReason {
    EndTurn,
    MaxTokens,
    MaxTurns,
    Cancelled,
}

impl From<Option<StopReason>> for DoneReason {
    /// We only ask at the end of a turn that called no tool, so a tool-use stop
    /// and a provider that says nothing both mean the same thing: it is over.
    fn from(reason: Option<StopReason>) -> Self {
        match reason {
            Some(StopReason::MaxTokens) => Self::MaxTokens,
            Some(StopReason::EndTurn | StopReason::ToolUse) | None => Self::EndTurn,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ThinkingBoundary,
    ToolPending {
        id: String,
        name: String,
    },
    ToolStart(Box<ToolStartEvent>),
    /// `content` is the **full accumulated output** so far, not a delta.
    /// Producers must accumulate into a growing buffer and send the whole thing each flush.
    ToolOutput {
        id: String,
        content: String,
    },
    ToolDone(Box<ToolDoneEvent>),
    BatchProgress(Box<BatchProgressEvent>),
    Question(Box<QuestionEvent>),
    GoalEvaluating {
        evaluation: u32,
    },
    GoalEvaluation {
        verdict: GoalVerdict,
        reason: String,
        evaluation: u32,
        applied: bool,
        usage: TokenUsage,
        cost: Option<f64>,
        model: String,
    },
    GoalFinished {
        result: GoalResult,
    },
    GoalDeferred {
        active_background_tasks: usize,
    },
    GoalLoopCap {
        evaluations: u32,
    },
    GoalTurnLimit {
        evaluations: u32,
    },
    GoalEvaluationFailed {
        evaluation: u32,
        message: String,
        applied: bool,
        usage: TokenUsage,
        cost: Option<f64>,
        model: String,
    },
    GoalClearedAfterError {
        condition: String,
        message: String,
    },
    TurnComplete(Box<TurnCompleteEvent>),
    ToolResultsSubmitted {
        message: Box<Message>,
    },
    QueueItemConsumed {
        id: crate::QueueItemId,
        text: String,
        image_count: usize,
    },
    QueueBatchConsumed {
        items: Vec<QueueConsumedItem>,
    },
    QueueDrained,
    Done {
        usage: TokenUsage,
        num_turns: u32,
        reason: DoneReason,
    },
    AutoCompacting,
    CompactionDone,
    Retry {
        attempt: u32,
        message: String,
        delay_ms: u64,
    },
    Error {
        message: String,
    },
    PermissionRequest(Box<PermissionRequest>),
    PermissionRequestResolved {
        request_id: String,
        source_request_id: String,
    },
    AuthRequired,
    Nudge,
    /// A subagent's progress moved. Only ever stamped with [`SubagentInfo`],
    /// so the parent knows which task header to update.
    SubagentProgress {
        progress: SubagentProgress,
    },
    SubagentHistory {
        task_id: String,
        parent_tool_use_id: String,
        root_tool_use_id: String,
        name: String,
        model: String,
        messages: Vec<Message>,
        spec: Option<caudra_storage::sessions::StoredSubagentTaskSpec>,
    },
    ToolSnapshot {
        id: String,
        snapshot: BufferSnapshot,
        /// Which theme baked these colors. `None` for live output.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme_gen: Option<u64>,
    },
    ToolHeaderSnapshot {
        id: String,
        snapshot: BufferSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme_gen: Option<u64>,
    },
    LiveToolBuf {
        id: String,
        body: Arc<SharedBuf>,
    },
    PromptProgress {
        processed: u32,
        total: u32,
        cache: u32,
    },
}

#[derive(Debug, Serialize)]
pub struct QueueConsumedItem {
    pub id: crate::QueueItemId,
    pub text: String,
    pub image_count: usize,
}

/// Append-only buffer for streaming tool output to the UI. Writers append
/// under a Mutex, readers get a cheap Arc clone via `read_if_dirty()`.
pub struct SharedBuf {
    committed: Mutex<Arc<Vec<SnapshotLine>>>,
    dirty: AtomicBool,
    on_change: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Opaque click handler owned by the Lua layer. It lives on the buffer
    /// itself, not on any one handle, so every handle wrapping this buf,
    /// even a foreign wrapper in another task, reaches the same handler.
    click: Mutex<Option<Arc<dyn Any + Send + Sync>>>,
    notifying: AtomicBool,
}

impl SharedBuf {
    pub fn new() -> Self {
        Self {
            committed: Mutex::new(Arc::new(Vec::new())),
            dirty: AtomicBool::new(false),
            on_change: Mutex::new(None),
            click: Mutex::new(None),
            notifying: AtomicBool::new(false),
        }
    }

    pub fn set_click(&self, f: Arc<dyn Any + Send + Sync>) {
        *self.click.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    pub fn click(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.click.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn clear_click(&self) {
        *self.click.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Fires synchronously after every `append`/`set_lines`, on the
    /// mutating thread. The callback must not mutate this buffer; recursive
    /// notifications are silently dropped. One slot only: a second call
    /// replaces the previous watcher.
    pub fn set_on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.on_change.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    /// A watcher keeps everything it captured alive for as long as it is
    /// installed, so owners must clear it once the watching task retires.
    pub fn clear_on_change(&self) {
        *self.on_change.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn notify_change(&self) {
        if self.notifying.swap(true, Ordering::AcqRel) {
            return;
        }
        let cb = self
            .on_change
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(cb) = cb {
            cb();
        }
        self.notifying.store(false, Ordering::Release);
    }

    pub fn append(&self, line: SnapshotLine) {
        let mut guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        Arc::make_mut(&mut guard).push(line);
        drop(guard);
        self.dirty.store(true, Ordering::Release);
        self.notify_change();
    }

    pub fn set_lines(&self, lines: Vec<SnapshotLine>) {
        let mut guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Arc::new(lines);
        drop(guard);
        self.dirty.store(true, Ordering::Release);
        self.notify_change();
    }

    pub fn len(&self) -> usize {
        self.committed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn read(&self) -> Arc<Vec<SnapshotLine>> {
        let guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(&guard)
    }

    pub fn read_if_dirty(&self) -> Option<Arc<Vec<SnapshotLine>>> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return None;
        }
        let guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        Some(Arc::clone(&guard))
    }

    /// A copy for a tool reply. Leaves the dirty flag alone: only the UI
    /// clears it, and a reply can die on the way there (a cancelled run's
    /// events are stale), which would strand the last repaint a tool made
    /// on its way out. Re-reading the same lines once costs nothing.
    pub fn take(&self) -> BufferSnapshot {
        let guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        BufferSnapshot::from_arc(Arc::clone(&guard))
    }
}

impl Default for SharedBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SharedBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBuf").finish_non_exhaustive()
    }
}

impl Serialize for SharedBuf {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_unit()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BufferSnapshot {
    pub lines: Arc<Vec<SnapshotLine>>,
}

impl BufferSnapshot {
    pub fn from_arc(lines: Arc<Vec<SnapshotLine>>) -> Self {
        Self { lines }
    }

    pub fn plain_text(text: String) -> Self {
        Self::from_arc(Arc::new(vec![SnapshotLine::plain(text)]))
    }

    pub fn first_line_text(&self) -> String {
        self.lines
            .first()
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
            .unwrap_or_default()
    }

    /// Search matches against this, so it must mirror exactly what the UI
    /// renders.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for span in &line.spans {
                out.push_str(&span.text);
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotLine {
    pub spans: Vec<SnapshotSpan>,
}

impl SnapshotLine {
    pub fn plain(text: String) -> Self {
        Self {
            spans: vec![SnapshotSpan {
                text,
                style: SpanStyle::Default,
            }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSpan {
    pub text: String,
    pub style: SpanStyle,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum SpanStyle {
    #[default]
    Default,
    Named(String),
    Inline(InlineStyle),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InlineStyle {
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub strikethrough: bool,
    pub reversed: bool,
}

#[derive(Debug, Serialize)]
pub struct TurnCompleteEvent {
    pub message: Message,
    pub usage: TokenUsage,
    pub model: String,
    #[serde(skip)]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_size: Option<u32>,
    /// The model's context window, so consumers can gauge `context_size`
    /// against the ceiling without resolving the model.
    pub context_window: u32,
}

/// A reasoning block that opens with a bold line names itself; everything
/// after that line is the thought proper. A block that does not follow the
/// shape is all body, so a stray `**` in prose is never mistaken for a title.
pub struct ReasoningSummary<'a> {
    pub title: Option<&'a str>,
    pub body: &'a str,
}

pub fn reasoning_summary(text: &str) -> ReasoningSummary<'_> {
    let content = text.trim();
    let untitled = ReasoningSummary {
        title: None,
        body: content,
    };
    let Some(after_open) = content.strip_prefix(THOUGHT_TITLE_FENCE) else {
        return untitled;
    };
    let Some(close) = after_open.find(THOUGHT_TITLE_FENCE) else {
        return untitled;
    };
    let title = after_open[..close].trim();
    if title.is_empty()
        || title
            .chars()
            .any(|character| matches!(character, '*' | '\n' | '\r'))
    {
        return untitled;
    }
    let suffix = &after_open[close + THOUGHT_TITLE_FENCE.len()..];
    let body = if suffix.is_empty() {
        ""
    } else if let Some(body) = suffix
        .strip_prefix(CRLF_PARAGRAPH_BREAK)
        .or_else(|| suffix.strip_prefix(PARAGRAPH_BREAK))
    {
        body.trim_end()
    } else {
        return untitled;
    };
    ReasoningSummary {
        title: Some(title),
        body,
    }
}

/// What a subagent is doing right now, so a parent watching only the task
/// header can tell a stalled run from a busy one. Derived from the child's own
/// event stream; the parent never inspects the child transcript for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "activity", rename_all = "snake_case")]
pub enum SubagentActivity {
    /// The block's own bold heading, once enough of it has streamed in.
    Thinking {
        title: Option<String>,
    },
    Responding,
    Tool {
        name: Arc<str>,
        summary: String,
    },
    Compacting,
    Retrying,
    AwaitingPermission,
}

impl SubagentActivity {
    /// A tool header can wrap or carry padding, and this renders on one line
    /// beside the spinner, so it is flattened once here rather than at each
    /// consumer.
    pub fn tool(name: Arc<str>, summary: &str) -> Self {
        Self::Tool {
            name,
            summary: summary.split_whitespace().collect::<Vec<_>>().join(" "),
        }
    }

    /// `None` for events that leave the activity as it was, so callers can
    /// treat every `Some` as a change worth publishing.
    pub fn from_event(event: &AgentEvent) -> Option<Self> {
        match event {
            // A title needs the whole block, which one delta does not carry;
            // whoever accumulates the stream fills it in.
            AgentEvent::ThinkingDelta { .. } => Some(Self::Thinking { title: None }),
            AgentEvent::TextDelta { .. } => Some(Self::Responding),
            // The input is still streaming, so there is no header to show yet.
            AgentEvent::ToolPending { name, .. } => Some(Self::tool(Arc::from(name.as_str()), "")),
            AgentEvent::ToolStart(start) => {
                Some(Self::tool(Arc::clone(&start.tool), &start.summary))
            }
            AgentEvent::AutoCompacting => Some(Self::Compacting),
            AgentEvent::Retry { .. } => Some(Self::Retrying),
            AgentEvent::PermissionRequest(_) => Some(Self::AwaitingPermission),
            _ => None,
        }
    }

    /// The leading word, styled like a tool prefix when it names one.
    pub fn label(&self) -> &str {
        match self {
            Self::Thinking { .. } => THINKING_LABEL,
            Self::Responding => RESPONDING_LABEL,
            Self::Tool { name, .. } => name,
            Self::Compacting => COMPACTING_LABEL,
            Self::Retrying => RETRYING_LABEL,
            Self::AwaitingPermission => AWAITING_PERMISSION_LABEL,
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Tool { summary, .. } if !summary.is_empty() => Some(summary),
            Self::Thinking { title } => title.as_deref(),
            _ => None,
        }
    }
}

/// A subagent's progress digest, published whenever any part of it changes.
/// `elapsed` is measured by the relay rather than by each consumer, so the
/// task header and a batch child row cannot disagree about the same run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubagentProgress {
    pub activity: SubagentActivity,
    /// Tool calls the subagent has started, the one in `activity` included.
    pub tools: u32,
    pub elapsed: Duration,
}

impl SubagentProgress {
    /// A subagent with nothing to tally yet reports only its clock, so a run
    /// still thinking does not claim "0 tools".
    ///
    /// `elapsed` is a parameter because a live consumer keeps counting past
    /// the last report, and both consumers must still spell it the same way.
    pub fn tally(tools: u32, elapsed: Duration) -> String {
        let clock = format_live_duration(elapsed);
        match tools {
            0 => clock,
            1 => format!("1 tool{TALLY_SEPARATOR}{clock}"),
            many => format!("{many} tools{TALLY_SEPARATOR}{clock}"),
        }
    }

    pub fn tally_now(&self) -> String {
        Self::tally(self.tools, self.elapsed)
    }
}

/// Tenth-second resolution, because this redraws while the clock runs and a
/// millisecond tail would be unreadable noise.
pub fn format_live_duration(duration: Duration) -> String {
    let tenths = duration.as_millis() / 100;
    let seconds = tenths / 10;
    if seconds < u128::from(SECONDS_PER_MINUTE) {
        format!("{}.{}s", seconds, tenths % 10)
    } else {
        format!(
            "{}m {}.{}s",
            seconds / u128::from(SECONDS_PER_MINUTE),
            seconds % u128::from(SECONDS_PER_MINUTE),
            tenths % 10
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentInfo {
    pub parent_tool_use_id: String,
    pub task_id: String,
    #[serde(rename = "parent_name")]
    pub name: String,
    #[serde(rename = "parent_prompt", skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "parent_model", skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip)]
    pub answer_tx: Option<flume::Sender<String>>,
    #[serde(skip)]
    pub steer_tx: Option<crate::SteeringQueue>,
}

#[derive(Debug, Clone)]
pub struct EventSender {
    tx: Sender<Envelope>,
    run_id: u64,
}

impl EventSender {
    pub fn new(tx: Sender<Envelope>, run_id: u64) -> Self {
        Self { tx, run_id }
    }

    pub fn send(&self, event: impl Into<AgentEvent>) -> Result<(), AgentError> {
        self.tx
            .try_send(Envelope {
                event: event.into(),
                subagent: None,
                run_id: self.run_id,
            })
            .map_err(|_| AgentError::Channel)
    }

    pub fn send_envelope(&self, envelope: Envelope) -> Result<(), AgentError> {
        self.tx.try_send(envelope).map_err(|_| AgentError::Channel)
    }

    pub fn try_send(&self, event: impl Into<AgentEvent>) {
        let _ = self.tx.try_send(Envelope {
            event: event.into(),
            subagent: None,
            run_id: self.run_id,
        });
    }

    pub fn run_id(&self) -> u64 {
        self.run_id
    }

    pub fn raw_tx(&self) -> &Sender<Envelope> {
        &self.tx
    }
}

#[derive(Debug, Serialize)]
pub struct Envelope {
    #[serde(flatten)]
    pub event: AgentEvent,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentInfo>,
    pub run_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_storage::StateDir;
    use caudra_storage::id::SessionRef;
    use caudra_storage::tool_outputs::ToolOutputStore;
    use tempfile::TempDir;
    use test_case::test_case;

    fn shell_output() -> ShellOutput {
        ShellOutput {
            model_text: "filtered summary\n\n[shell status: exit code 0]".into(),
            relative_workdir: ".".into(),
            timeout_ms: 120_000,
            duration_ms: 10,
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            output_limit_exceeded: false,
            final_sequence: 2,
            stdout_utf8_bytes: 14,
            stderr_utf8_bytes: 10,
            stdout: "raw stdout".into(),
            stderr: "raw stderr".into(),
            stdout_capture_truncated: false,
            stderr_capture_truncated: false,
            stdout_preview_truncated: false,
            stderr_preview_truncated: false,
            stdout_redraws_collapsed: 0,
            stderr_redraws_collapsed: 0,
            filter: Some(ShellFilterInfo {
                stages: vec!["cargo".into()],
                unfiltered_utf8_bytes: 100,
                filtered_utf8_bytes: 20,
            }),
        }
    }

    #[test]
    fn shell_output_separates_model_and_raw_projections() {
        let output = ToolOutput::Shell(shell_output());

        assert_eq!(
            output.as_text(),
            "filtered summary\n\n[shell status: exit code 0]"
        );
        assert_eq!(
            output.as_display_text(),
            "stdout tail:\nraw stdout\nstderr tail:\nraw stderr"
        );
        assert_eq!(output.annotation().as_deref(), Some("exit 0"));
    }

    #[test_case(ToolOutput::Plain("ok".into()),                      Some("1 lines")     ; "plain_short_annotates")]
    #[test_case(ToolOutput::Plain((0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n").into()), Some("20 lines") ; "plain_long_annotates")]
    #[test_case(ToolOutput::Plain(String::new().into()),             None                ; "plain_empty_no_annotation")]
    #[test_case(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 1, lines: vec!["x".into(); 5], total_lines: 5, instructions: None }, Some("5 lines") ; "read_code_full_file")]
    #[test_case(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 10, lines: vec!["x".into(); 5], total_lines: 100, instructions: None }, Some("5 of 100 lines") ; "read_code_partial")]
    #[test_case(ToolOutput::WriteCode { path: "a.rs".into(), byte_count: 99, lines: vec!["x".into(); 3] }, Some("3 lines") ; "write_code_lines")]
    #[test_case(ToolOutput::WriteCode { path: "a.rs".into(), byte_count: 99, lines: vec![] }, Some("99 bytes") ; "write_code_falls_back_for_old_sessions")]
    #[test_case(ToolOutput::WriteCode { path: "a.rs".into(), byte_count: 0, lines: vec![] }, Some("0 lines") ; "write_code_empty_file")]
    #[test_case(ToolOutput::GrepResult { entries: vec![GrepFileEntry { path: "a.rs".into(), groups: vec![GrepMatchGroup::single(1, "hit")] }], capped: None }, Some("1 matches in 1 file") ; "grep_file_count")]
    #[test_case(ToolOutput::GrepResult { entries: vec![GrepFileEntry { path: "a.rs".into(), groups: vec![GrepMatchGroup::single(1, "hit")] }], capped: Some(SearchCap { files_scanned: 40, files_listed: 900 }) }, Some("1 matches in 1 file (capped, 40/900 searched)") ; "grep_capped_reports_how_far_it_got")]
    #[test_case(ToolOutput::Diff { path: "a.rs".into(), before: "a\nb\n".into(), after: "a\nc\nd\n".into(), summary: "ok".into() }, Some("+2 -1") ; "diff_counts_both_sides")]
    #[test_case(ToolOutput::Diff { path: "a.rs".into(), before: String::new(), after: "new\n".into(), summary: "ok".into() }, Some("+1 -0") ; "diff_pure_insert")]
    fn annotation_cases(output: ToolOutput, expected: Option<&str>) {
        assert_eq!(output.annotation().as_deref(), expected);
    }

    #[test]
    fn truncated_index_directory_annotation_is_a_lower_bound() {
        let output = ToolOutput::Index(IndexOutput::Directory {
            path: "/project".into(),
            relative_path: ".".into(),
            entries: Vec::new(),
            total_count: 10_000,
            truncated: true,
            listing: String::new(),
            instructions: None,
            state: None,
        });

        assert_eq!(
            output.annotation().as_deref(),
            Some("at least 10000 entries")
        );
    }

    #[test_case(None ; "no_stop_reason")]
    #[test_case(Some(StopReason::ToolUse) ; "tool_use")]
    fn stop_reason_without_its_own_ending_becomes_end_turn(stop: Option<StopReason>) {
        assert_eq!(DoneReason::from(stop), DoneReason::EndTurn);
    }

    #[test]
    fn clear_on_change_stops_notifications() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let buf = SharedBuf::new();
        let fired = Arc::new(AtomicUsize::new(0));
        let f = Arc::clone(&fired);
        buf.set_on_change(move || {
            f.fetch_add(1, Ordering::SeqCst);
        });
        buf.append(SnapshotLine { spans: vec![] });
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        buf.clear_on_change();
        buf.append(SnapshotLine { spans: vec![] });
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn legacy_batch_output_with_entries_still_deserializes() {
        let json = r#"{"Batch":{"entries":[{"tool":"read","summary":"s","status":"Success","input":null,"output":null}],"text":"stored"}}"#;
        let out: ToolOutput =
            serde_json::from_str(json).expect("old persisted session JSON must load");
        assert_eq!(out.as_text(), "stored");
    }

    /// Search and the UI's error-dedup guard depend on this exact shape:
    /// spans join bare, lines join with one newline, no trailing newline.
    #[test]
    fn buffer_snapshot_text_joins_spans_and_lines() {
        let snap = BufferSnapshot::from_arc(Arc::new(vec![
            SnapshotLine {
                spans: vec![
                    SnapshotSpan {
                        text: "1 ".into(),
                        style: SpanStyle::Named("line_nr".into()),
                    },
                    SnapshotSpan {
                        text: "print('hi')".into(),
                        style: SpanStyle::Default,
                    },
                ],
            },
            SnapshotLine { spans: vec![] },
            SnapshotLine::plain("out".into()),
        ]));
        assert_eq!(snap.text(), "1 print('hi')\n\nout");
        assert_eq!(BufferSnapshot::from_arc(Arc::new(vec![])).text(), "");
    }

    #[test]
    fn as_display_text_diff_renders_unified_text() {
        let output = ToolOutput::Diff {
            path: "src/main.rs".into(),
            before: "keep\nold\n".into(),
            after: "keep\nnew\n".into(),
            summary: "Updated value".into(),
        };
        let display = output.as_display_text();
        assert!(display.starts_with("Updated value"));
        assert!(display.contains("--- src/main.rs"));
        assert!(display.contains("+++ src/main.rs"));
        assert!(display.contains("  keep"));
        assert!(display.contains("- old"));
        assert!(display.contains("+ new"));
        assert_eq!(output.as_text(), "Updated value");
    }

    #[test]
    fn as_text_grep_result_multi_file() {
        let output = ToolOutput::GrepResult {
            entries: vec![
                GrepFileEntry {
                    path: "src/a.rs".into(),
                    groups: vec![
                        GrepMatchGroup::single(3, "fn foo()"),
                        GrepMatchGroup::single(10, "fn bar()"),
                    ],
                },
                GrepFileEntry {
                    path: "src/b.rs".into(),
                    groups: vec![GrepMatchGroup::single(1, "use crate")],
                },
            ],
            capped: None,
        };
        let text = output.as_text();
        assert!(text.contains("src/a.rs"));
        assert!(text.contains("3: fn foo()"));
        assert!(text.contains("10: fn bar()"));
        assert!(text.contains("src/b.rs"));
        assert!(text.contains("1: use crate"));
    }

    #[test]
    fn as_text_grep_result_with_context() {
        let output = ToolOutput::GrepResult {
            entries: vec![GrepFileEntry {
                path: "src/a.rs".into(),
                groups: vec![
                    GrepMatchGroup {
                        lines: vec![
                            GrepLine::context(2, "let x = 1;"),
                            GrepLine::matched(3, "fn foo()"),
                            GrepLine::context(4, "let y = 2;"),
                        ],
                    },
                    GrepMatchGroup::single(20, "fn bar()"),
                ],
            }],
            capped: None,
        };
        let text = output.as_text();
        assert!(text.contains("2  let x = 1;"), "context before: {text}");
        assert!(text.contains("3: fn foo()"), "match line: {text}");
        assert!(text.contains("4  let y = 2;"), "context after: {text}");
        assert!(text.contains("--"), "group separator: {text}");
        assert!(text.contains("20: fn bar()"), "second group: {text}");
    }

    #[test_case(ToolOutput::WriteCode { path: "src/lib.rs".into(), byte_count: 10, lines: vec![] }, Some("src/lib.rs") ; "write_code")]
    #[test_case(ToolOutput::Diff { path: "src/lib.rs".into(), before: String::new(), after: String::new(), summary: String::new() }, Some("src/lib.rs") ; "diff")]
    #[test_case(ToolOutput::Plain("ok".into()), None ; "non_write_variant")]
    fn output_written_path(output: ToolOutput, expected: Option<&str>) {
        assert_eq!(output.written_path(), expected);
    }

    #[test]
    fn tool_results_builds_message_with_tool_result_blocks() {
        let msg = tool_results(vec![
            ToolDoneEvent {
                id: "t1".into(),
                tool: Arc::from("bash"),
                output: ToolOutput::Plain("ok".into()),
                is_error: false,
                annotation: None,
                written_path: None,
                written_paths: Vec::new(),
                output_ref: None,
                output_limits: None,
                model_suffix: None,
                model_output: None,
                model_output_from_ref: false,
            },
            ToolDoneEvent {
                id: "t2".into(),
                tool: Arc::from("read"),
                output: ToolOutput::Plain("fail".into()),
                is_error: true,
                annotation: None,
                written_path: None,
                written_paths: Vec::new(),
                output_ref: None,
                output_limits: None,
                model_suffix: None,
                model_output: None,
                model_output_from_ref: false,
            },
        ]);
        assert!(matches!(msg.role, Role::User));
        assert_eq!(msg.content.len(), 2);
        assert!(
            matches!(&msg.content[0], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "t1" && !is_error)
        );
        assert!(
            matches!(&msg.content[1], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "t2" && *is_error)
        );
    }

    #[test]
    fn tool_results_appends_model_suffix_on_success_and_error() {
        let done = |id: &str, output: &str, is_error: bool, suffix: &str| {
            ToolDoneEvent {
                id: id.into(),
                tool: Arc::from("test"),
                output: ToolOutput::Plain(output.into()),
                is_error,
                annotation: None,
                written_path: None,
                written_paths: Vec::new(),
                output_ref: None,
                output_limits: None,
                model_suffix: None,
                model_output: None,
                model_output_from_ref: false,
            }
            .with_model_suffix(Some(suffix.into()))
        };
        let msg = tool_results(vec![
            done("ok", "visible\n\n", false, "\nmodel context\n"),
            done("err", "failed", true, "recovery hint"),
        ]);

        assert!(
            matches!(&msg.content[0], ContentBlock::ToolResult { content, is_error, .. }
                if content == "visible\n\nmodel context" && !is_error)
        );
        assert!(
            matches!(&msg.content[1], ContentBlock::ToolResult { content, is_error, .. }
                if content == "failed\n\nrecovery hint" && *is_error)
        );
    }

    #[test]
    fn tool_results_prefers_bounded_model_output_and_attaches_output_ref() {
        let temp = TempDir::new().unwrap();
        let store = ToolOutputStore::new(StateDir::from_path(temp.path().to_path_buf()));
        let session = SessionRef::generate();
        let output_ref = store
            .put(session.id(), "full output\n\nmodel context")
            .unwrap();
        let done = ToolDoneEvent {
            id: "t1".into(),
            tool: Arc::from("test"),
            output: ToolOutput::Plain("presentation preview".into()),
            is_error: false,
            annotation: None,
            written_path: None,
            written_paths: Vec::new(),
            output_ref: Some(output_ref.clone()),
            output_limits: None,
            model_suffix: Some("model context".into()),
            model_output: Some("bounded preview\n\nmodel context".into()),
            model_output_from_ref: false,
        };

        let serialized = serde_json::to_value(&done).unwrap();
        assert_eq!(serialized["output_ref"]["id"], output_ref.id.to_string());
        assert!(serialized.get("model_output").is_none());
        assert!(serialized.get("model_suffix").is_none());
        assert!(serialized.get("output_limits").is_none());

        let message = tool_results(vec![done]);
        assert!(matches!(
            &message.content[0],
            ContentBlock::ToolResult {
                content,
                output_ref: Some(actual_ref),
                ..
            } if content == "bounded preview\n\nmodel context"
                && content.matches("model context").count() == 1
                && actual_ref == &output_ref
        ));
    }

    #[test]
    fn tool_done_model_suffix_preserves_path_and_is_not_serialized() {
        const MODEL_SUFFIX: &str = "internal model context";

        let done = ToolDoneEvent {
            id: "t1".into(),
            tool: Arc::from("write"),
            output: ToolOutput::Plain("wrote file".into()),
            is_error: false,
            annotation: None,
            written_path: Some("/tmp/file.rs".into()),
            written_paths: Vec::new(),
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
        }
        .with_model_suffix(Some(MODEL_SUFFIX.into()));

        assert_eq!(done.written_path(), Some("/tmp/file.rs"));
        assert_eq!(done.model_suffix(), Some(MODEL_SUFFIX));
        let json = serde_json::to_string(&done).unwrap();
        assert!(json.contains(r#""written_path":"/tmp/file.rs""#));
        assert!(!json.contains("written_paths"));
        assert!(json.contains(r#""output_ref":null"#));
        assert!(!json.contains("model_suffix"));
        assert!(!json.contains("model_output"));
        assert!(!json.contains(MODEL_SUFFIX));
    }

    #[test]
    fn tool_results_appends_images_after_all_results() {
        let image = |data: &str| ToolOutput::Image {
            source: caudra_providers::ImageSource::new(
                caudra_providers::ImageMediaType::Png,
                Arc::from(data),
            ),
            text: "[image: pic.png 1KB]".into(),
        };
        let done = |id: &str, output: ToolOutput| ToolDoneEvent {
            id: id.into(),
            tool: Arc::from("t"),
            output,
            is_error: false,
            annotation: None,
            written_path: None,
            written_paths: Vec::new(),
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
        };

        let msg = tool_results(vec![
            done("t1", image("aGVsbG8=")),
            done("t2", ToolOutput::Plain("ok".into())),
            done("t3", image("aW1n")),
        ]);
        assert_eq!(msg.content.len(), 5);
        assert!(
            matches!(&msg.content[0], ContentBlock::ToolResult { tool_use_id, content, .. } if tool_use_id == "t1" && content == "[image: pic.png 1KB]")
        );
        assert!(
            matches!(&msg.content[1], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t2")
        );
        assert!(
            matches!(&msg.content[2], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t3")
        );
        assert!(
            matches!(&msg.content[3], ContentBlock::Image { source } if &*source.data == "aGVsbG8=")
        );
        assert!(
            matches!(&msg.content[4], ContentBlock::Image { source } if &*source.data == "aW1n")
        );
        assert_eq!(msg.tool_result_image_owners, ["t1", "t3"]);
        assert!(
            serde_json::to_value(msg)
                .unwrap()
                .get("tool_result_image_owners")
                .is_none()
        );
    }

    #[test_case(
        10,
        vec!["fn foo()".into(), "fn bar()".into()],
        Some(vec![InstructionBlock { path: "AGENTS.md".into(), content: "do stuff".into() }]),
        "10: fn foo()\n11: fn bar()\n\n...\n\nTruncated lines: 12-100. Use offset=12 to read further."
        ; "with_instructions"
    )]
    #[test_case(
        1,
        vec!["line1".into()],
        None,
        "1: line1\n\n...\n\nTruncated lines: 2-100. Use offset=2 to read further."
        ; "without_instructions"
    )]
    fn read_code_display_text(
        start_line: usize,
        lines: Vec<String>,
        instructions: Option<Vec<InstructionBlock>>,
        expected: &str,
    ) {
        let output = ToolOutput::ReadCode {
            path: "a.rs".into(),
            start_line,
            lines,
            total_lines: 100,
            instructions,
        };
        assert_eq!(output.as_display_text(), expected);
    }

    #[test]
    fn read_code_as_text_includes_instructions() {
        let output = ToolOutput::ReadCode {
            path: "a.rs".into(),
            start_line: 1,
            lines: vec!["fn main()".into()],
            total_lines: 1,
            instructions: Some(vec![InstructionBlock {
                path: "AGENTS.md".into(),
                content: "do stuff".into(),
            }]),
        };
        let text = output.as_text();
        assert!(text.contains("1: fn main()"));
        assert!(text.contains("Instructions from: AGENTS.md"));
        assert!(text.contains("do stuff"));
    }

    #[test]
    fn wrote_to_checks_path_and_error_flag() {
        let ok_event = ToolDoneEvent {
            id: "id".into(),
            tool: Arc::from("write"),
            output: ToolOutput::Plain("wrote 10 bytes".into()),
            is_error: false,
            annotation: None,
            written_path: Some("/plans/slug.md".into()),
            written_paths: Vec::new(),
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
        };
        assert!(!ok_event.wrote_to(Path::new("/plans/other.md")));

        let err_event = ToolDoneEvent {
            is_error: true,
            ..ok_event
        };
        assert!(!err_event.wrote_to(Path::new("/plans/slug.md")));
    }

    #[test]
    fn plural_written_paths_keep_singular_compatibility() {
        let event = ToolDoneEvent {
            id: "id".into(),
            tool: Arc::from("patch"),
            output: ToolOutput::Plain("patched".into()),
            is_error: false,
            annotation: None,
            written_path: Some("/project/first.rs".into()),
            written_paths: vec!["/project/first.rs".into(), "/project/second.rs".into()],
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
        };

        assert_eq!(event.written_path(), Some("/project/first.rs"));
        assert_eq!(
            event.written_paths().collect::<Vec<_>>(),
            ["/project/first.rs", "/project/second.rs"]
        );
        assert!(event.wrote_to(Path::new("/project/second.rs")));
        let serialized = serde_json::to_value(&event).unwrap();
        assert_eq!(serialized["written_path"], "/project/first.rs");
        assert_eq!(
            serialized["written_paths"],
            serde_json::json!(["/project/first.rs", "/project/second.rs"])
        );
    }

    #[test]
    fn read_code_backward_compat_deserialization() {
        let json = r#"{"ReadCode":{"path":"a.rs","start_line":1,"lines":["x"]}}"#;
        let output: ToolOutput = serde_json::from_str(json).unwrap();
        match output {
            ToolOutput::ReadCode {
                total_lines,
                instructions,
                ..
            } => {
                assert_eq!(total_lines, 0);
                assert!(instructions.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test_case(100, 10, 2, 89 ; "middle_of_file")]
    #[test_case(100, 1, 1, 99  ; "first_line_only")]
    #[test_case(5, 1, 5, 0     ; "all_lines_shown")]
    #[test_case(5, 1, 2, 3     ; "partial_from_start")]
    #[test_case(5, 3, 3, 0     ; "partial_to_end")]
    #[test_case(0, 1, 1, 0     ; "backward_compat_total_zero")]
    #[test_case(0, 1, 0, 0     ; "empty_lines_total_zero")]
    #[test_case(10, 10, 1, 0   ; "last_line")]
    fn lines_remaining(total: usize, start: usize, shown: usize, expected: usize) {
        assert_eq!(lines_remaining_after(total, start, shown), expected);
    }

    fn line(text: &str) -> SnapshotLine {
        SnapshotLine {
            spans: vec![SnapshotSpan {
                text: text.into(),
                style: SpanStyle::Default,
            }],
        }
    }

    #[test]
    fn shared_buf_lifecycle() {
        let buf = SharedBuf::new();

        assert!(buf.is_empty());
        assert!(buf.read_if_dirty().is_none());

        for i in 0..3 {
            buf.append(line(&format!("l{i}")));
        }
        assert_eq!(buf.len(), 3);

        let snap = buf.read_if_dirty().expect("dirty after appends");
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].spans[0].text, "l0");
        assert!(buf.read_if_dirty().is_none(), "clean after read");

        buf.append(line("l3"));
        let _ = buf.take();
        assert!(
            buf.read_if_dirty().is_some(),
            "take must leave the flag for the UI"
        );
    }

    #[test]
    fn shared_buf_arc_snapshot_isolation() {
        let buf = SharedBuf::new();
        buf.append(line("a"));
        buf.append(line("b"));
        let snap = buf.read_if_dirty().unwrap();
        buf.append(line("c"));
        assert_eq!(snap.len(), 2, "held Arc must not see new appends");
        let snap2 = buf.read_if_dirty().unwrap();
        assert_eq!(snap2.len(), 3);
    }

    #[test]
    fn shared_buf_poisoned_mutex_recovery() {
        let buf = Arc::new(SharedBuf::new());
        let buf2 = Arc::clone(&buf);
        let h = std::thread::spawn(move || {
            let _guard = buf2.committed.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = h.join();
        buf.append(SnapshotLine { spans: vec![] });
    }

    #[test]
    fn buffer_snapshot_first_line_text() {
        let empty = BufferSnapshot {
            lines: Arc::new(vec![]),
        };
        assert_eq!(empty.first_line_text(), "");

        let multi = BufferSnapshot {
            lines: Arc::new(vec![SnapshotLine {
                spans: vec![
                    SnapshotSpan {
                        text: "hello ".into(),
                        style: SpanStyle::Default,
                    },
                    SnapshotSpan {
                        text: "world".into(),
                        style: SpanStyle::Named("bold".into()),
                    },
                ],
            }]),
        };
        assert_eq!(multi.first_line_text(), "hello world");
    }

    #[test_case(SpanStyle::Default ; "default")]
    #[test_case(SpanStyle::Named("comment".into()) ; "named")]
    #[test_case(SpanStyle::Inline(InlineStyle {
        fg: Some((255, 0, 0)),
        bg: None,
        bold: true,
        italic: false,
        underline: true,
        dim: false,
        strikethrough: false,
        reversed: true,
    }) ; "inline")]
    fn snapshot_span_serde_roundtrip(style: SpanStyle) {
        let span = SnapshotSpan {
            text: "test".into(),
            style,
        };
        let json = serde_json::to_string(&span).unwrap();
        let parsed: SnapshotSpan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, span);
    }

    #[test_case("", true  ; "plain_output_is_empty_for_empty_string")]
    #[test_case("a.rs\nb.rs", false ; "plain_output_not_empty_for_content")]
    fn plain_output_is_empty(text: &str, expected: bool) {
        assert_eq!(ToolOutput::Plain(text.into()).is_empty_result(), expected);
    }

    #[test]
    fn agent_event_tool_snapshot_theme_gen_backwards_compat() {
        const OMIT_MSG: &str = "theme_gen: None must not appear in serialized JSON";
        const COMPAT_MSG: &str = "missing theme_gen must deserialize as None (backwards compat)";

        let event = AgentEvent::ToolSnapshot {
            id: "t1".into(),
            snapshot: BufferSnapshot {
                lines: Arc::new(vec![]),
            },
            theme_gen: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("theme_gen"), "{OMIT_MSG}");

        #[derive(Deserialize)]
        struct ToolSnapshotFields {
            #[allow(dead_code)]
            id: String,
            #[serde(default)]
            theme_gen: Option<u64>,
        }
        let json_without = r#"{"id":"t1"}"#;
        let parsed: ToolSnapshotFields = serde_json::from_str(json_without).unwrap();
        assert_eq!(parsed.theme_gen, None, "{COMPAT_MSG}");
    }

    #[test]
    fn text_output_serde_full_roundtrip() {
        const MSG: &str = "new format with instructions must roundtrip";
        let blocks = vec![InstructionBlock {
            path: "AGENTS.md".into(),
            content: "be nice".into(),
        }];
        let output = ToolOutput::Plain(TextOutput {
            text: "file contents".into(),
            instructions: Some(blocks),
            state: None,
            lua_provenance: Some(LuaToolProvenance {
                plugin: "test".into(),
                contract: "contract".into(),
                error_restore_allowed: true,
            }),
        });
        let json = serde_json::to_string(&output).unwrap();
        let parsed: ToolOutput = serde_json::from_str(&json).unwrap();
        match &parsed {
            ToolOutput::Plain(t) => {
                assert_eq!(t.text, "file contents", "{MSG}");
                let inst = t.instructions.as_ref().expect("instructions missing");
                assert_eq!(inst.len(), 1, "{MSG}");
                assert_eq!(inst[0].path, "AGENTS.md", "{MSG}");
                assert_eq!(inst[0].content, "be nice", "{MSG}");
                assert_eq!(
                    t.lua_provenance,
                    Some(LuaToolProvenance {
                        plugin: "test".into(),
                        contract: "contract".into(),
                        error_restore_allowed: true,
                    }),
                    "{MSG}"
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test_case(
        ToolOutput::WriteCode { path: "/old/path".into(), byte_count: 10, lines: vec![] },
        Some("/new/path".into()), false, Some("/new/path")
        ; "prefers_field_over_output"
    )]
    #[test_case(
        ToolOutput::Diff { path: "/diff/path".into(), before: String::new(), after: String::new(), summary: String::new() },
        None, false, Some("/diff/path")
        ; "falls_back_to_output"
    )]
    #[test_case(
        ToolOutput::Plain("failed".into()), Some("/some/path".into()), true, None
        ; "none_when_error"
    )]
    fn tool_done_written_path(
        output: ToolOutput,
        written_path: Option<String>,
        is_error: bool,
        expected: Option<&str>,
    ) {
        let event = ToolDoneEvent {
            id: "id".into(),
            tool: Arc::from("tool"),
            output,
            is_error,
            annotation: None,
            written_path,
            written_paths: Vec::new(),
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
        };
        assert_eq!(event.written_path(), expected);
    }

    #[test]
    fn plain_with_instructions_as_text_includes_instructions() {
        const INCLUDES_MSG: &str = "as_text must include instructions";
        const EXCLUDES_MSG: &str = "as_display_text must exclude instructions";
        let output = ToolOutput::Plain(TextOutput {
            text: "fn main()".into(),
            instructions: Some(vec![InstructionBlock {
                path: "AGENTS.md".into(),
                content: "do stuff".into(),
            }]),
            state: None,
            lua_provenance: None,
        });
        let text = output.as_text();
        assert!(text.contains("fn main()"), "{INCLUDES_MSG}");
        assert!(
            text.contains("Instructions from: AGENTS.md"),
            "{INCLUDES_MSG}"
        );
        assert!(text.contains("do stuff"), "{INCLUDES_MSG}");

        let display = output.as_display_text();
        assert!(display.contains("fn main()"), "{EXCLUDES_MSG}");
        assert!(!display.contains("Instructions from:"), "{EXCLUDES_MSG}");
    }

    fn tool_start(tool: &str, summary: &str) -> AgentEvent {
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "toolu_01".into(),
            effect: ToolEffect::Unknown,
            tool: Arc::from(tool),
            summary: summary.into(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
        }))
    }

    #[test_case(
        AgentEvent::ThinkingDelta { text: "**Weighing it up**".into() },
        Some((THINKING_LABEL, None))
        ; "one_delta_cannot_name_the_block_it_opens"
    )]
    #[test_case(
        AgentEvent::TextDelta { text: "hi".into() },
        Some((RESPONDING_LABEL, None))
        ; "text_delta"
    )]
    #[test_case(
        AgentEvent::ToolPending { id: "toolu_01".into(), name: "shell".into() },
        Some(("shell", None))
        ; "pending_tool_has_no_header_yet"
    )]
    #[test_case(
        tool_start("shell", "cargo nextest run"),
        Some(("shell", Some("cargo nextest run")))
        ; "started_tool_carries_its_header"
    )]
    #[test_case(
        tool_start("shell", "  rg -n\n  'AgentEvent'  "),
        Some(("shell", Some("rg -n 'AgentEvent'")))
        ; "a_wrapped_header_collapses_to_one_line"
    )]
    #[test_case(AgentEvent::AutoCompacting, Some((COMPACTING_LABEL, None)) ; "compacting")]
    #[test_case(
        AgentEvent::Retry { attempt: 1, message: "overloaded".into(), delay_ms: 10 },
        Some((RETRYING_LABEL, None))
        ; "retrying"
    )]
    #[test_case(AgentEvent::CompactionDone, None ; "unrelated_event_leaves_it_unchanged")]
    #[test_case(AgentEvent::Nudge, None ; "nudge_leaves_it_unchanged")]
    fn activity_reads_the_childs_own_events(
        event: AgentEvent,
        expected: Option<(&str, Option<&str>)>,
    ) {
        let activity = SubagentActivity::from_event(&event);
        assert_eq!(
            activity
                .as_ref()
                .map(|activity| (activity.label(), activity.detail())),
            expected
        );
    }

    #[test_case("**Weighing it up**", Some("Weighing it up"), "" ; "a_title_alone_has_no_body_yet")]
    #[test_case(
        "**Weighing it up**\n\nBoth read the same file.",
        Some("Weighing it up"),
        "Both read the same file."
        ; "a_blank_line_separates_the_two"
    )]
    #[test_case(
        "**Weighing it up**\r\n\r\nBoth read the same file.",
        Some("Weighing it up"),
        "Both read the same file."
        ; "crlf_separates_them_too"
    )]
    #[test_case(
        "**Weighing it up**\nBoth read the same file.",
        None,
        "**Weighing it up**\nBoth read the same file."
        ; "one_newline_is_emphasis_mid_sentence_not_a_heading"
    )]
    #[test_case("**Weighing", None, "**Weighing" ; "an_unclosed_fence_names_nothing")]
    #[test_case("Both read the same file.", None, "Both read the same file." ; "plain_prose")]
    #[test_case("****\n\nbody", None, "****\n\nbody" ; "an_empty_title_is_not_one")]
    #[test_case("**a**b**\n\nbody", None, "**a**b**\n\nbody" ; "a_stray_star_disqualifies_it")]
    fn a_thought_is_named_only_by_a_leading_bold_line(text: &str, title: Option<&str>, body: &str) {
        let summary = reasoning_summary(text);
        assert_eq!(summary.title, title);
        assert_eq!(summary.body, body);
    }
}
