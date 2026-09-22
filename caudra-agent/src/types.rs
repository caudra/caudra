use std::any::Any;
use std::fmt::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use caudra_providers::{
    AgentError, Billing, ContentBlock, Message, Role, StopReason, TokenUsage, token_label,
};
use caudra_storage::tool_ledger::ToolOutcome;
use caudra_storage::tool_outputs::ToolOutputRef;
use caudra_storage::usage_ledger::LedgerPurpose;
use caudra_workflow::{
    AgentRosterEntry, LogLine, PhaseRecord, RosterState, RunSnapshot, RunStatus, RunUsage,
};
use caudra_workspace::LocalDocumentRef;
use flume::Sender;
use serde::{Deserialize, Serialize};
use strum::Display;

use crate::agent::{GoalResult, GoalVerdict};
use crate::permissions::PermissionRequest;
use crate::tools::ToolEffect;

pub const NO_FILES_FOUND: &str = "No files found";
pub const INDEX_TRUNCATED: &str = "[truncated]";
/// The `run_id` every [`AgentEvent::Workflow`] envelope carries. Workflow runs
/// outlive agent turns, so they never belong to one. One below the UI's
/// restore sentinel, which already claims `u64::MAX`.
pub const WORKFLOW_EVENT_RUN_ID: u64 = u64::MAX - 1;
/// How much of a run's report or result a transcript card quotes.
pub const MAX_CARD_PREVIEW_BYTES: usize = 2048;
/// Log lines a card keeps under its roster while the run works.
pub const CARD_LOG_LINES: usize = 3;
const CARD_REPORT_FIELD: &str = "report";
const CARD_PREVIEW_MARKER: &str = "…";
const CARD_PHASE_SEPARATOR: &str = " › ";
const CARD_ANNOTATION_SEPARATOR: &str = " · ";

/// Labels for the two command rows an environment result ends with. The card
/// and the model text use the same words, so a reader and the model are looking
/// at the same thing.
pub const ENVIRONMENT_COMMANDS_LABEL: &str = "commands";
pub const ENVIRONMENT_MISSING_LABEL: &str = "missing";
/// What an installed command with no parseable version reads as. A version
/// parser declining the output says nothing about the command being there.
pub const ENVIRONMENT_NO_VERSION: &str = "(no version)";
const ENVIRONMENT_LIST_SEPARATOR: &str = ", ";

/// How a memory browse names the notes directory. Only a browse reports it,
/// because it is how the model reaches a note with `file_edit`.
pub const MEMORY_DIRECTORY_LABEL: &str = "dir: ";
/// How a note held by reference names itself, in both the card and the text.
pub const MEMORY_REFERENCE_LABEL: &str = "memory_ref: ";
pub const MEMORY_REVISION_LABEL: &str = "revision: ";
pub const MEMORY_TAG_SEPARATOR: &str = ", ";
const MEMORY_INDEX_BULLET: &str = "  - ";
const MEMORY_NOTE_SEPARATOR: &str = "\n\n";
const MEMORY_INDEX_SEPARATOR: &str = "\n";
const MEMORY_NOTE_NOUN: &str = "note";
const MEMORY_TAG_NOUN: &str = "tag";

const MILLIS_PER_SECOND: u128 = 1_000;
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
/// What each phase is called once a later one has replaced it. A tool answers
/// the same question from its own inflection table; a phase has no table, so
/// its two spellings live beside each other here.
const THINKING_PAST_LABEL: &str = "thought";
const RESPONDING_PAST_LABEL: &str = "responded";
const COMPACTING_PAST_LABEL: &str = "compacted";
const RETRYING_PAST_LABEL: &str = "retried";
const AWAITING_PERMISSION_PAST_LABEL: &str = "awaited permission";

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
///
/// `question` and `options` are what the pick was made against, so a card can
/// redraw the form rather than the picks alone. Both are skipped by serde
/// rather than stored: the tool call's input holds the form already, a second
/// copy under the result could only disagree with it, and a restored answer is
/// filled back in from the input. That is also what lets a session written
/// before the card drew a form render the same as one written after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub header: String,
    pub labels: Vec<String>,
    #[serde(skip)]
    pub question: String,
    #[serde(skip)]
    pub options: Vec<QuestionOption>,
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
    /// What the child was allowed to do, stamped from the child's own start
    /// event so the card reads the same however long after the run it is
    /// opened. A roster entry that has not started yet carries `Unknown`, as
    /// does one restored from a session written before this was recorded.
    #[serde(default)]
    pub effect: ToolEffect,
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
    /// Guidance the child addressed to the model alone, such as the task id a
    /// `task` child hands back to be resumed with. Kept apart from the
    /// annotation because that one is read by both sides, and folding the two
    /// together drew a metadata block on the card. Lives only until the batch
    /// assembles its answer, which is the text that is persisted.
    #[serde(skip)]
    pub model_suffix: Option<String>,
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

/// One symbol in a code-graph answer.
///
/// `inbound`/`outbound` are reference counts and `hops` is a distance, so a row
/// carries whichever its tool measured and leaves the rest unset rather than
/// reporting a zero it did not compute. Every count is a floor: edges are
/// recovered from source text by name, so dynamic dispatch and macros
/// contribute none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeGraphRow {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbound: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbound: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hops: Option<usize>,
    #[serde(default)]
    pub test_scope: bool,
}

impl CodeGraphRow {
    /// The plain-text form, used when a card cannot be drawn.
    pub fn as_line(&self) -> String {
        let mut line = String::new();
        if let Some(hops) = self.hops {
            line.push_str(&format!("hop {hops} "));
        }
        line.push_str(&format!(
            "{} {} {}:{}-{}",
            self.name, self.kind, self.path, self.line_start, self.line_end
        ));
        if let (Some(inbound), Some(outbound)) = (self.inbound, self.outbound) {
            line.push_str(&format!(" in={inbound} out={outbound}"));
        }
        if self.test_scope {
            line.push_str(" [test]");
        }
        line
    }
}

/// One labelled line of an environment card, e.g. `packages` / `apt 2.8.3`.
/// The adapter decides what a fact is called and how its value reads, so the
/// card and the model text share one vocabulary and neither restates the
/// host's JSON key names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentFact {
    pub label: String,
    pub value: String,
}

/// One probed command. `available` and `version` answer different questions: a
/// command can resolve and start yet print nothing a version parser accepts, so
/// a missing version is never evidence of a missing command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentCommand {
    pub id: String,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// The body `code_expand` returns alongside its neighbours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeGraphSource {
    pub path: String,
    pub kind: String,
    pub line_start: usize,
    pub lines: Vec<String>,
    /// Set when the whole file was returned instead of the symbol, and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whole_file_reason: Option<String>,
}

/// Where a note lives: what a click can open, and what the model needs to
/// change it. A remote workspace holds notes by reference, so there is no host
/// path to name and the reference and its revision take that place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MemoryOrigin {
    File { path: String },
    Document { reference: String, revision: String },
}

impl MemoryOrigin {
    /// The note's file on this host, or `None` for one held by reference.
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::File { path } => Some(path),
            Self::Document { .. } => None,
        }
    }

    /// The lines that name a remote note, which is how the model reaches it
    /// with `local_document_read` and replaces it with `local_document_write`.
    fn locator_lines(&self) -> Vec<String> {
        match self {
            Self::File { .. } => Vec::new(),
            Self::Document {
                reference,
                revision,
            } => Vec::from([
                format!("{MEMORY_REFERENCE_LABEL}{reference}"),
                format!("{MEMORY_REVISION_LABEL}{revision}"),
            ]),
        }
    }

    /// What an index row appends to name a note it cannot name by path.
    fn index_suffix(&self) -> String {
        match self {
            Self::File { .. } => String::new(),
            Self::Document { reference, .. } => format!(" [{MEMORY_REFERENCE_LABEL}{reference}]"),
        }
    }
}

/// One note a read returned: what it is called, what its body costs, what
/// reaches it, and where it lives. The body has its frontmatter stripped, so
/// `tags` is the only place tags are stated and the token count is what the
/// model is actually charged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryNote {
    pub name: String,
    pub tokens: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub origin: MemoryOrigin,
    pub body: String,
}

impl MemoryNote {
    /// `name (1.2k tokens) [arch, ui]`, the row a card draws and the line the
    /// model reads the body under.
    pub fn headline(&self) -> String {
        let tags = match self.tags.is_empty() {
            true => String::new(),
            false => format!(" [{}]", self.tags.join(MEMORY_TAG_SEPARATOR)),
        };
        format!("{} ({}){tags}", self.name, token_label(self.tokens))
    }

    fn as_text(&self) -> String {
        let mut out = vec![self.headline()];
        out.extend(self.origin.locator_lines());
        format!("{}\n\n{}", out.join("\n"), self.body)
    }
}

/// One note in a tag index. A list never reads a body, so it carries none
/// rather than an empty one it would have to be believed about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryNoteEntry {
    pub name: String,
    pub tokens: u32,
    pub origin: MemoryOrigin,
}

impl MemoryNoteEntry {
    fn as_text(&self) -> String {
        format!(
            "{MEMORY_INDEX_BULLET}{} ({}){}",
            self.name,
            token_label(self.tokens),
            self.origin.index_suffix()
        )
    }
}

/// A tag and the notes it reaches. A note carrying several tags appears under
/// each of them, which is how `/memory` and the prompt's tag line already
/// present the same index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryTagGroup {
    pub tag: String,
    pub notes: Vec<MemoryNoteEntry>,
}

/// What a memory browse answered with: whole notes for `read`, the tag index
/// that reaches them for `list`.
///
/// `notices` are the lines that qualify the answer — tags ignored, files that
/// would not read, nothing matched — and they lead, so a card can draw them
/// apart from the notes and the model reads them before what they qualify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MemoryOutput {
    Notes {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        directory: Option<String>,
        notes: Vec<MemoryNote>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notices: Vec<String>,
    },
    Index {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        directory: Option<String>,
        groups: Vec<MemoryTagGroup>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notices: Vec<String>,
    },
}

impl MemoryOutput {
    pub fn directory(&self) -> Option<&str> {
        let (Self::Notes { directory, .. } | Self::Index { directory, .. }) = self;
        directory.as_deref()
    }

    pub fn notices(&self) -> &[String] {
        let (Self::Notes { notices, .. } | Self::Index { notices, .. }) = self;
        notices
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Notes { notes, .. } => notes.is_empty(),
            Self::Index { groups, .. } => groups.is_empty(),
        }
    }

    /// `3 notes · 4.2k tokens` for a read, `18 notes · 7 tags` for a list.
    /// A list counts distinct notes, because one carrying three tags is filed
    /// three times and is still one note.
    pub fn annotation(&self) -> String {
        match self {
            Self::Notes { notes, .. } => format!(
                "{}{CARD_ANNOTATION_SEPARATOR}{}",
                counted(notes.len(), MEMORY_NOTE_NOUN),
                token_label(notes.iter().map(|note| note.tokens).sum())
            ),
            Self::Index { groups, .. } => {
                let mut names: Vec<&str> = groups
                    .iter()
                    .flat_map(|group| group.notes.iter().map(|note| note.name.as_str()))
                    .collect();
                names.sort_unstable();
                names.dedup();
                format!(
                    "{}{CARD_ANNOTATION_SEPARATOR}{}",
                    counted(names.len(), MEMORY_NOTE_NOUN),
                    counted(groups.len(), MEMORY_TAG_NOUN)
                )
            }
        }
    }

    /// The one rendering of this answer as text: what the model reads, and what
    /// a reader copies out of the card.
    ///
    /// The two shapes separate their parts differently because a note's body is
    /// prose that needs air around it, while an index is already a list.
    pub fn as_display_text(&self) -> String {
        let (separator, body) = match self {
            Self::Notes { notes, .. } => (
                MEMORY_NOTE_SEPARATOR,
                notes.iter().map(MemoryNote::as_text).collect::<Vec<_>>(),
            ),
            Self::Index { groups, .. } => (
                MEMORY_INDEX_SEPARATOR,
                groups
                    .iter()
                    .map(|group| {
                        let mut rows = vec![
                            format!("{} ({})", group.tag, group.notes.len()),
                            String::new(),
                        ];
                        rows.splice(1..1, group.notes.iter().map(MemoryNoteEntry::as_text));
                        rows.join("\n")
                    })
                    .collect(),
            ),
        };
        let directory = self
            .directory()
            .map(|directory| format!("{MEMORY_DIRECTORY_LABEL}{directory}"));
        let listed = self
            .notices()
            .iter()
            .cloned()
            .chain(body)
            .collect::<Vec<_>>()
            .join(separator);
        match directory {
            Some(directory) => format!("{directory}{MEMORY_NOTE_SEPARATOR}{listed}"),
            None => listed,
        }
    }
}

/// `1 note`, `3 notes`. Small enough to inline everywhere it is needed, and
/// wrong often enough when it is.
fn counted(count: usize, noun: &str) -> String {
    match count {
        1 => format!("{count} {noun}"),
        _ => format!("{count} {noun}s"),
    }
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

/// A workflow run as its transcript card shows it: the parts of a snapshot
/// a reader follows, with the report cut to what a card can hold. Stored
/// with the tool result so a restored session draws the card in Rust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunCard {
    pub run_id: String,
    pub display_name: String,
    pub workflow_name: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default)]
    pub phases: Vec<String>,
    #[serde(default)]
    pub phase_history: Vec<PhaseRecord>,
    pub agent_budget: u32,
    #[serde(default)]
    pub usage: RunUsage,
    #[serde(default)]
    pub roster: Vec<AgentRosterEntry>,
    #[serde(default)]
    pub logs: Vec<LogLine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl From<&RunSnapshot> for WorkflowRunCard {
    fn from(run: &RunSnapshot) -> Self {
        let result_preview = run.result.as_ref().map(|result| {
            let text = result
                .get(CARD_REPORT_FIELD)
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| result.to_string(), str::to_owned);
            if text.len() <= MAX_CARD_PREVIEW_BYTES {
                text
            } else {
                let end = text.floor_char_boundary(MAX_CARD_PREVIEW_BYTES);
                format!("{}{CARD_PREVIEW_MARKER}", &text[..end])
            }
        });
        let logs = run
            .logs
            .iter()
            .rev()
            .take(CARD_LOG_LINES)
            .rev()
            .cloned()
            .collect();
        Self {
            run_id: run.run_id.clone(),
            display_name: run.display_name.clone(),
            workflow_name: run.workflow_name.clone(),
            status: run.status,
            phase: run.phase.clone(),
            phases: run.phases.clone(),
            phase_history: run.phase_history.clone(),
            agent_budget: run.agent_budget,
            usage: run.usage.clone(),
            roster: run.roster.clone(),
            logs,
            result_preview,
            scratch_path: run.scratch_path().map(str::to_owned),
            pause_message: run.pause_message.clone(),
            error: run.error.clone(),
            created_at: run.created_at,
            updated_at: run.updated_at,
        }
    }
}

impl WorkflowRunCard {
    /// `status`, then the phase when the run is in one.
    pub fn headline(&self) -> String {
        match &self.phase {
            Some(phase) => format!("{}{CARD_ANNOTATION_SEPARATOR}{phase}", self.status),
            None => self.status.to_string(),
        }
    }

    /// The declared phases in order with the current one marked, or the
    /// phases seen so far when the script declared none.
    pub fn phase_strip(&self) -> Vec<(String, PhaseMark)> {
        let titles: Vec<&str> = if self.phases.is_empty() {
            self.phase_history
                .iter()
                .map(|record| record.title.as_str())
                .collect()
        } else {
            self.phases.iter().map(String::as_str).collect()
        };
        let current = self
            .phase
            .as_deref()
            .and_then(|phase| titles.iter().rposition(|title| *title == phase));
        let settled = self.status.is_terminal();
        titles
            .iter()
            .enumerate()
            .map(|(index, title)| {
                let mark = match current {
                    Some(at) if index < at => PhaseMark::Done,
                    Some(at) if index == at && settled => PhaseMark::Done,
                    Some(at) if index == at => PhaseMark::Current,
                    _ => PhaseMark::Pending,
                };
                ((*title).to_owned(), mark)
            })
            .collect()
    }

    pub fn running_agents(&self) -> impl Iterator<Item = &AgentRosterEntry> {
        self.roster
            .iter()
            .filter(|agent| agent.state == RosterState::Running)
    }

    fn display_text(&self) -> String {
        let mut out = format!(
            "{} ({}) {}",
            self.display_name,
            self.workflow_name,
            self.headline()
        );
        let strip: Vec<String> = self
            .phase_strip()
            .into_iter()
            .map(|(title, mark)| format!("{title} {}", mark.glyph()))
            .collect();
        if !strip.is_empty() {
            out.push('\n');
            out.push_str(&strip.join(CARD_PHASE_SEPARATOR));
        }
        for agent in &self.roster {
            let _ = write!(out, "\n  {} [{}]", agent.label, agent.state);
        }
        if let Some(preview) = &self.result_preview {
            out.push('\n');
            out.push_str(preview);
        }
        if let Some(error) = &self.error {
            let _ = write!(out, "\nerror: {error}");
        }
        out
    }
}

/// Where a phase stands in a card's strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseMark {
    Done,
    Current,
    Pending,
}

impl PhaseMark {
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Done => "✓",
            Self::Current => "●",
            Self::Pending => "○",
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
    /// What a memory browse found. Carried structurally rather than as the
    /// markdown it used to be, so a card can tell one note from the next and a
    /// collapsed one can still say which notes came back.
    Memory(MemoryOutput),
    /// A code-graph answer: a headline, ranked or reached rows, an optional
    /// body, and the footer describing the graph they came from.
    ///
    /// `annotation` is supplied per call because the rows mean different things
    /// per tool — callers, reached symbols, ranked symbols — and one derived
    /// count would be wrong for four of the five.
    CodeGraph {
        headline: String,
        rows: Vec<CodeGraphRow>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<CodeGraphSource>,
        footer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<serde_json::Value>,
    },
    Shell(ShellOutput),
    /// What the host looks like right now. `headline` and `summary` are the two
    /// lines that make the rest interpretable, so a collapsed card shows them
    /// and nothing else; `facts` are the labelled rows under them.
    Environment {
        headline: String,
        summary: String,
        facts: Vec<EnvironmentFact>,
        commands: Vec<EnvironmentCommand>,
    },
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
    /// A workflow run the transcript follows live and restores settled.
    WorkflowRun(Box<WorkflowRunCard>),
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
    /// Whether the patch was cut short of the change it describes.
    ///
    /// Workcell shortens a receipt past its byte bound rather than refusing the
    /// mutation, so the counts above can describe more than the patch shows.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
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
                start_line,
                lines,
                total_lines,
                ..
            } => {
                let shown = lines.len();
                Some(
                    if *total_lines == 0 || (*start_line == 1 && shown >= *total_lines) {
                        format!("{shown} lines")
                    } else if shown == 0 {
                        format!("0 of {total_lines} lines")
                    } else {
                        let end = start_line
                            .saturating_add(shown)
                            .saturating_sub(1)
                            .min(*total_lines);
                        format!("lines {start_line}–{end} of {total_lines}")
                    },
                )
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
            Self::Memory(output) => Some(output.annotation()),
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
            Self::WorkflowRun(card) => Some(card.headline()),
            Self::Environment { headline, .. } => Some(headline.clone()),
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
            | Self::Index(IndexOutput::Directory { state, .. })
            | Self::CodeGraph { state, .. } => state.as_ref(),
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
            | Self::Memory(_)
            | Self::CodeGraph { .. }
            | Self::Shell(_)
            | Self::TodoList(_)
            | Self::Answers(_)
            | Self::Environment { .. }
            | Self::WorkflowRun(_) => Some(self.as_display_text()),
            _ => None,
        }
    }

    pub fn is_empty_result(&self) -> bool {
        match self {
            Self::GrepResult { entries, .. } => entries.is_empty(),
            Self::Index(IndexOutput::File { skeleton, .. }) => skeleton.is_empty(),
            Self::Index(IndexOutput::Directory { listing, .. }) => listing.is_empty(),
            Self::Shell(output) => output.stdout.is_empty() && output.stderr.is_empty(),
            Self::Memory(output) => output.is_empty(),
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
            Self::CodeGraph {
                headline,
                rows,
                source,
                footer,
                ..
            } => {
                let mut out = vec![headline.clone()];
                out.extend(rows.iter().map(CodeGraphRow::as_line));
                if let Some(source) = source {
                    out.push(format!("{}:{}", source.path, source.line_start));
                    out.extend(source.lines.iter().cloned());
                }
                out.push(footer.clone());
                out.join("\n")
            }
            Self::Environment {
                headline,
                summary,
                facts,
                commands,
            } => {
                let mut out = vec![headline.clone(), summary.clone()];
                out.extend(
                    facts
                        .iter()
                        .map(|fact| format!("{}: {}", fact.label, fact.value)),
                );
                let present: Vec<String> = commands
                    .iter()
                    .filter(|command| command.available)
                    .map(|command| match &command.version {
                        Some(version) => format!("{} {version}", command.id),
                        None => format!("{} {ENVIRONMENT_NO_VERSION}", command.id),
                    })
                    .collect();
                if !present.is_empty() {
                    out.push(format!(
                        "{ENVIRONMENT_COMMANDS_LABEL}: {}",
                        present.join(ENVIRONMENT_LIST_SEPARATOR)
                    ));
                }
                let missing: Vec<&str> = commands
                    .iter()
                    .filter(|command| !command.available)
                    .map(|command| command.id.as_str())
                    .collect();
                if !missing.is_empty() {
                    out.push(format!(
                        "{ENVIRONMENT_MISSING_LABEL}: {}",
                        missing.join(ENVIRONMENT_LIST_SEPARATOR)
                    ));
                }
                out.join("\n")
            }
            Self::Index(IndexOutput::File { skeleton, .. }) => skeleton.clone(),
            Self::Index(IndexOutput::Directory { listing, .. }) => listing.clone(),
            Self::Memory(output) => output.as_display_text(),
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
            Self::WorkflowRun(card) => card.display_text(),
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote_written_paths: bool,
    pub output_ref: Option<ToolOutputRef>,
    #[serde(skip)]
    pub output_limits: Option<ToolOutputLimits>,
    #[serde(skip)]
    pub model_suffix: Option<String>,
    #[serde(skip)]
    pub model_output: Option<String>,
    #[serde(skip)]
    pub model_output_from_ref: bool,
    /// What the call cost in wall clock, how it ended, where the tool came
    /// from, and how much of the context window its result took. Filled once,
    /// after bounding, so telemetry and the durable ledger cannot disagree.
    /// Skipped by serde: this is host accounting, not part of any transcript or
    /// protocol.
    #[serde(skip)]
    pub accounting: ToolAccounting,
}

#[derive(Debug, Clone, Default)]
pub struct ToolAccounting {
    pub duration_ms: u64,
    pub source: Option<Arc<str>>,
    pub outcome: Option<ToolOutcome>,
    pub model_tokens: u32,
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
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            accounting: ToolAccounting::default(),
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
        !self.remote_written_paths
            && self
                .written_paths()
                .any(|written_path| Path::new(written_path) == plan_path)
    }

    pub fn wrote_document(&self, reference: &LocalDocumentRef) -> bool {
        if self.is_error {
            return false;
        }
        let (kind, id) = match reference {
            LocalDocumentRef::Plan(reference) => ("plan", reference.as_str()),
            LocalDocumentRef::Memory(reference) => ("memory", reference.as_str()),
        };
        self.annotation.as_deref().is_some_and(|annotation| {
            annotation
                .strip_prefix("local_document:")
                .and_then(|value| value.split_once(";revision:"))
                .is_some_and(|(written, _)| written == format!("{kind}:{id}"))
        })
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
    /// One fragment of a still-streaming tool call's arguments, with the one
    /// scalar worth showing before the call is complete.
    ToolInputDelta {
        id: String,
        name: String,
        /// The raw JSON fragment, for consumers that replay the wire.
        delta: String,
        /// `Some` only when the preview changed, so every `Some` is a render
        /// and a long argument does not repaint the row per token.
        preview: Option<String>,
        /// How much of a file body has arrived, e.g. `120+ lines`. `Some` only
        /// when the floor moved, which is once per step rather than per token.
        size: Option<String>,
        /// What this fragment added to the file being written, decoded out of
        /// `delta`. Only a whole-file write publishes a body: every other
        /// change is legible as a diff and as nothing else, so it is counted
        /// into `size` and drawn once the call has run. `None` once a body
        /// outgrows what a card can hold.
        #[serde(skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        /// The children a `batch` has named so far, as pending roster rows.
        /// `Some` only when a row was added or one changed, so a long list
        /// does not redraw the card per token. `ToolStart` replaces it with
        /// the roster the batch actually dispatched.
        #[serde(skip_serializing_if = "Option::is_none")]
        roster: Option<Vec<BatchToolEntry>>,
        /// The briefs the delegating calls in this fragment are writing: the
        /// call itself, or the `batch` children that moved. Empty for every
        /// fragment that delegates nothing, which is nearly all of them.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        delegations: Vec<Delegation>,
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
        billing: Billing,
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
        continuations: u32,
        limit: u32,
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
        billing: Billing,
        model: String,
    },
    GoalClearedAfterError {
        condition: String,
        message: String,
    },
    TurnComplete(Box<TurnCompleteEvent>),
    ModelUsage {
        usage: TokenUsage,
        cost: Option<f64>,
        billing: Billing,
        provider: String,
        model: String,
        purpose: LedgerPurpose,
    },
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
    Compacting,
    CompactionDone,
    /// A model-written name for the session, produced off the turn's critical
    /// path. Arrives at most once per session and may land after the run that
    /// triggered it has finished.
    ///
    /// Carries its own spend rather than riding [`AgentEvent::TurnComplete`]:
    /// the title never enters the conversation, so reporting it as a turn
    /// would overwrite the context size with a request that is not in context.
    SessionTitle {
        /// `None` when the model answered with nothing usable. The spend still
        /// has to be reported, so the event fires either way.
        title: Option<String>,
        usage: TokenUsage,
        cost: Option<f64>,
        billing: Billing,
        model: String,
        provider: String,
    },
    Retry {
        attempt: u32,
        message: String,
        delay_ms: u64,
    },
    Error {
        message: String,
    },
    PermissionRequest(Box<PermissionRequest>),
    PermissionRequestUpdated(Box<PermissionRequest>),
    PermissionRequestResolved {
        request_id: String,
        source_request_id: String,
    },
    StreamReset,
    AuthRequired,
    AuthRestored,
    /// A stall is being answered. The counts let one row report the budget
    /// draining instead of the same line arriving once per attempt.
    Nudge {
        attempt: u32,
        limit: u32,
    },
    /// A message the harness wrote into the conversation on the user's behalf:
    /// a standing reminder, a goal check-in, a nudge, a continuation. Reported
    /// so the transcript can show what was sent rather than only that something
    /// was. `@mention` file bodies are excluded; the user already sees the path.
    Injected {
        text: String,
    },
    /// Deferred tools moved into the request array. Reported because the user
    /// is paying for it: the tools array changes, so the provider's prompt
    /// cache prefix is invalidated and the next request re-reads the history.
    ToolsLoaded {
        names: Vec<String>,
    },
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
    ToolAnnotation {
        id: String,
        annotation: String,
    },
    PromptProgress {
        processed: u32,
        total: u32,
        cache: u32,
    },
    /// A workflow run moved. Always sent under [`WORKFLOW_EVENT_RUN_ID`] with
    /// the run's provenance on the envelope.
    Workflow(Box<caudra_workflow::WorkflowEvent>),
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
    /// Named separately from `model`, which stays the bare id the UI shows.
    /// A tiered workload can resolve to another provider entirely, and the
    /// spend belongs to whoever billed it.
    pub provider: String,
    /// Why this call happened. Compaction rides the same event as the
    /// conversation and must not be billed as if it were the conversation.
    #[serde(skip)]
    pub purpose: LedgerPurpose,
    #[serde(skip)]
    pub cost: Option<f64>,
    /// Who owes `cost`. A subscription turn is priced at API rates for
    /// reporting, so the number is only meaningful next to its payer.
    #[serde(skip)]
    pub billing: Billing,
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
    if title.is_empty() || title.chars().any(breaks_a_title) {
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

/// What a still-animating block has earned so far, where `visible` is the
/// revealed prefix of `buffered`. The buffer decides which half the reveal is
/// spelling out, so a heading whose fence has not closed yet shows nothing at
/// all rather than drawing its own markup as body text and taking it back a
/// frame later.
pub fn streaming_reasoning_summary<'a>(visible: &'a str, buffered: &str) -> ReasoningSummary<'a> {
    if !may_be_titled(buffered) {
        return ReasoningSummary {
            title: None,
            body: visible.trim(),
        };
    }
    let revealed = reasoning_summary(visible);
    if revealed.title.is_none() {
        return ReasoningSummary {
            title: None,
            body: "",
        };
    }
    revealed
}

/// Whether a prefix can still turn out to name itself. An unclosed fence is
/// undecided rather than plain, and only text that has already broken the
/// shape is body for good.
fn may_be_titled(prefix: &str) -> bool {
    let content = prefix.trim_start();
    let Some(after_open) = content.strip_prefix(THOUGHT_TITLE_FENCE) else {
        return THOUGHT_TITLE_FENCE.starts_with(content);
    };
    let Some(close) = after_open.find(THOUGHT_TITLE_FENCE) else {
        // A single trailing star is the closing fence arriving, not a stray.
        let started = after_open.strip_suffix('*').unwrap_or(after_open);
        return !started.chars().any(breaks_a_title);
    };
    let title = after_open[..close].trim();
    if title.is_empty() || title.chars().any(breaks_a_title) {
        return false;
    }
    let suffix = &after_open[close + THOUGHT_TITLE_FENCE.len()..];
    suffix.starts_with(PARAGRAPH_BREAK)
        || suffix.starts_with(CRLF_PARAGRAPH_BREAK)
        || PARAGRAPH_BREAK.starts_with(suffix)
        || CRLF_PARAGRAPH_BREAK.starts_with(suffix)
}

fn breaks_a_title(character: char) -> bool {
    matches!(character, '*' | '\n' | '\r')
}

/// One child of a batch a subagent is running, as its parent's progress row
/// draws it. Deliberately not [`BatchToolEntry`], which carries the input and
/// the whole output: this is republished every time any child changes state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActivityChild {
    pub tool: Arc<str>,
    pub summary: String,
    pub status: BatchToolStatus,
}

impl From<&BatchToolEntry> for ActivityChild {
    fn from(entry: &BatchToolEntry) -> Self {
        Self {
            tool: Arc::from(entry.tool.as_str()),
            summary: entry
                .summary
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            status: entry.status,
        }
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        /// The roster, when the tool is a `batch`. Empty for every other call.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        children: Vec<ActivityChild>,
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
            call_id: None,
            children: Vec::new(),
        }
    }

    /// The same row, with the roster the batch behind it is working through.
    pub fn batch(name: Arc<str>, summary: &str, children: Vec<ActivityChild>) -> Self {
        match Self::tool(name, summary) {
            Self::Tool { name, summary, .. } => Self::Tool {
                name,
                summary,
                call_id: None,
                children,
            },
            other => other,
        }
    }

    pub fn with_call_id(mut self, id: &str) -> Self {
        if let Self::Tool { call_id, .. } = &mut self {
            *call_id = Some(id.to_owned());
        }
        self
    }

    /// The batch roster behind this activity, empty for everything else.
    pub fn children(&self) -> &[ActivityChild] {
        match self {
            Self::Tool { children, .. } => children,
            _ => &[],
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
            // The input is still streaming, so the header is whatever the
            // arguments have revealed so far: nothing at first, then the one
            // scalar the preview pulled out of the fragments.
            AgentEvent::ToolPending { id, name } => {
                Some(Self::tool(Arc::from(name.as_str()), "").with_call_id(id))
            }
            AgentEvent::ToolInputDelta {
                id, name, preview, ..
            } => preview
                .as_deref()
                .map(|preview| Self::tool(Arc::from(name.as_str()), preview).with_call_id(id)),
            AgentEvent::ToolStart(start) => {
                Some(Self::tool(Arc::clone(&start.tool), &start.summary).with_call_id(&start.id))
            }
            AgentEvent::Compacting => Some(Self::Compacting),
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

    /// The same word for a phase a later activity has replaced. A tool is
    /// absent because its own inflection table already spells all three tenses.
    pub fn past_label(&self) -> &str {
        match self {
            Self::Thinking { .. } => THINKING_PAST_LABEL,
            Self::Responding => RESPONDING_PAST_LABEL,
            Self::Tool { name, .. } => name,
            Self::Compacting => COMPACTING_PAST_LABEL,
            Self::Retrying => RETRYING_PAST_LABEL,
            Self::AwaitingPermission => AWAITING_PERMISSION_PAST_LABEL,
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

/// The same clock once it has stopped, which can afford a millisecond tail
/// because nothing redraws it.
pub fn format_settled_duration(duration: Duration) -> String {
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

/// What a delegating call has revealed about the subagent it is about to
/// open, while its arguments are still arriving. The chat opened from this is
/// a prediction: [`SubagentInfo`] is the truth, and it adopts the chat by
/// `parent_tool_use_id`, which is exact in every case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Delegation {
    /// The id the subagent's events will carry: the call's own `tool_use_id`,
    /// or the one a `batch` derives for the child writing this.
    pub parent_tool_use_id: String,
    /// The `description` so far, republished as it grows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What this fragment added to the prompt, decoded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Set only by a call continuing a subagent that already has an id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
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
    /// The level this task's own requests carry, already snapped against its
    /// own model. `None` when that model cannot reason at all.
    #[serde(rename = "parent_thinking", skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(rename = "parent_fast", skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    #[serde(skip)]
    pub answer_tx: Option<flume::Sender<String>>,
    #[serde(skip)]
    pub steer_tx: Option<crate::SteeringQueue>,
}

/// Which workflow run an event belongs to, so a consumer can attribute an
/// agent the workflow engine launched to the run and call that launched it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowProvenance {
    #[serde(rename = "workflow_run_id")]
    pub run_id: String,
    #[serde(rename = "workflow_epoch")]
    pub epoch: u64,
    #[serde(rename = "workflow_call_key")]
    pub call_key: u64,
    #[serde(rename = "workflow_phase", skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EventSender {
    tx: Sender<Envelope>,
    run_id: u64,
    workflow: Option<WorkflowProvenance>,
}

impl EventSender {
    pub fn new(tx: Sender<Envelope>, run_id: u64) -> Self {
        Self {
            tx,
            run_id,
            workflow: None,
        }
    }

    /// Every envelope sent from here on carries `workflow`.
    pub fn with_workflow(mut self, workflow: WorkflowProvenance) -> Self {
        self.workflow = Some(workflow);
        self
    }

    /// The same run and provenance on another channel.
    pub fn rebind(&self, tx: Sender<Envelope>) -> Self {
        Self {
            tx,
            run_id: self.run_id,
            workflow: self.workflow.clone(),
        }
    }

    pub fn send(&self, event: impl Into<AgentEvent>) -> Result<(), AgentError> {
        self.tx
            .try_send(self.envelope(event.into()))
            .map_err(|_| AgentError::Channel)
    }

    /// An envelope that already names its workflow keeps it: a relayed child
    /// event may come from a run other than this sender's.
    pub fn send_envelope(&self, mut envelope: Envelope) -> Result<(), AgentError> {
        if envelope.workflow.is_none() {
            envelope.workflow = self.workflow.clone();
        }
        self.tx.try_send(envelope).map_err(|_| AgentError::Channel)
    }

    pub fn try_send(&self, event: impl Into<AgentEvent>) {
        let _ = self.tx.try_send(self.envelope(event.into()));
    }

    pub fn run_id(&self) -> u64 {
        self.run_id
    }

    pub fn workflow(&self) -> Option<&WorkflowProvenance> {
        self.workflow.as_ref()
    }

    pub fn raw_tx(&self) -> &Sender<Envelope> {
        &self.tx
    }

    fn envelope(&self, event: AgentEvent) -> Envelope {
        Envelope {
            event,
            subagent: None,
            run_id: self.run_id,
            workflow: self.workflow.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Envelope {
    #[serde(flatten)]
    pub event: AgentEvent,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentInfo>,
    pub run_id: u64,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowProvenance>,
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

    const EXPECT_UNCLASSIFIED: &str =
        "a roster written before effects were recorded reads as unclassified, not as read-only";

    /// Restored sessions carry the roster verbatim, so the field has to be
    /// optional on the wire. `chat` fills the gap back in from the registry.
    #[test]
    fn a_child_recorded_without_an_effect_is_unclassified() {
        let entry: BatchToolEntry = serde_json::from_value(serde_json::json!({
            "tool": "file_read",
            "summary": "lib.rs",
            "status": "Success",
        }))
        .expect("a roster from an older session still loads");

        assert_eq!(entry.effect, ToolEffect::Unknown, "{EXPECT_UNCLASSIFIED}");
    }

    const ENVIRONMENT_HEADLINE: &str = "ubuntu 24.04 · linux/x86_64";
    const ENVIRONMENT_SUMMARY: &str = "bash · container sandbox · not root";
    const ENVIRONMENT_TEXT: &str = "ubuntu 24.04 · linux/x86_64\n\
         bash · container sandbox · not root\n\
         runtime: workcell-mcp 0.1.0\n\
         commands: bash 5.2.21, kubectl (no version)\n\
         missing: zsh";

    /// A command that ran but named no version is still a command that is
    /// there, and the rendering has to keep those two facts apart.
    #[test]
    fn an_environment_result_renders_as_labelled_lines() {
        let output = ToolOutput::Environment {
            headline: ENVIRONMENT_HEADLINE.into(),
            summary: ENVIRONMENT_SUMMARY.into(),
            facts: vec![EnvironmentFact {
                label: "runtime".into(),
                value: "workcell-mcp 0.1.0".into(),
            }],
            commands: vec![
                EnvironmentCommand {
                    id: "bash".into(),
                    available: true,
                    version: Some("5.2.21".into()),
                },
                EnvironmentCommand {
                    id: "kubectl".into(),
                    available: true,
                    version: None,
                },
                EnvironmentCommand {
                    id: "zsh".into(),
                    available: false,
                    version: None,
                },
            ],
        };

        assert_eq!(output.as_text(), ENVIRONMENT_TEXT);
        assert_eq!(output.annotation().as_deref(), Some(ENVIRONMENT_HEADLINE));
    }

    const MEMORY_DIRECTORY: &str = "/notes";
    const MEMORY_BODY: &str = "the body";
    const MEMORY_TAG: &str = "workcell";

    fn memory_note(name: &str, tokens: u32) -> MemoryNote {
        MemoryNote {
            name: name.to_owned(),
            tokens,
            tags: Vec::from([MEMORY_TAG.to_owned()]),
            origin: MemoryOrigin::File {
                path: format!("{MEMORY_DIRECTORY}/{name}"),
            },
            body: MEMORY_BODY.to_owned(),
        }
    }

    /// Lines are what a markdown blob could count, and they said nothing about
    /// a browse. Notes and what they cost are what a reader is deciding on.
    #[test]
    fn a_read_annotates_itself_with_notes_and_what_they_cost() {
        let output = ToolOutput::Memory(MemoryOutput::Notes {
            directory: Some(MEMORY_DIRECTORY.into()),
            notes: Vec::from([memory_note("a.md", 400), memory_note("b.md", 600)]),
            notices: Vec::new(),
        });

        assert_eq!(
            output.annotation().as_deref(),
            Some(format!("2 notes{CARD_ANNOTATION_SEPARATOR}{}", token_label(1_000)).as_str())
        );
    }

    /// A note filed under three tags is listed three times and is still one
    /// note, so the count a reader is given is of notes rather than of rows.
    #[test]
    fn a_list_annotates_distinct_notes_rather_than_filings() {
        let entry = MemoryNoteEntry {
            name: "a.md".into(),
            tokens: 1,
            origin: MemoryOrigin::File {
                path: format!("{MEMORY_DIRECTORY}/a.md"),
            },
        };
        let output = ToolOutput::Memory(MemoryOutput::Index {
            directory: Some(MEMORY_DIRECTORY.into()),
            groups: Vec::from([
                MemoryTagGroup {
                    tag: MEMORY_TAG.into(),
                    notes: Vec::from([entry.clone()]),
                },
                MemoryTagGroup {
                    tag: "ui".into(),
                    notes: Vec::from([entry]),
                },
            ]),
            notices: Vec::new(),
        });

        assert_eq!(
            output.annotation().as_deref(),
            Some(format!("1 note{CARD_ANNOTATION_SEPARATOR}2 tags").as_str())
        );
    }

    /// The directory leads, because it is how the model reaches a note with
    /// `file_edit`, and a notice qualifies what follows it rather than trailing
    /// the note it was about.
    #[test]
    fn a_browse_renders_as_the_one_text_both_sides_read() {
        const NOTICE: &str = "warning: unreadable memory files: c.md";
        let output = ToolOutput::Memory(MemoryOutput::Notes {
            directory: Some(MEMORY_DIRECTORY.into()),
            notes: Vec::from([memory_note("a.md", 1)]),
            notices: Vec::from([NOTICE.to_owned()]),
        });

        assert_eq!(
            output.as_text(),
            format!(
                "{MEMORY_DIRECTORY_LABEL}{MEMORY_DIRECTORY}\n\n\
                 {NOTICE}\n\n\
                 a.md ({}) [{MEMORY_TAG}]\n\n{MEMORY_BODY}",
                token_label(1)
            )
        );
    }

    /// A card is rebuilt from the stored output when a session reopens, so the
    /// structure has to survive the store rather than the text it replaced.
    #[test]
    fn a_browse_survives_being_stored_and_reopened() {
        let output = ToolOutput::Memory(MemoryOutput::Notes {
            directory: Some(MEMORY_DIRECTORY.into()),
            notes: Vec::from([memory_note("a.md", 1)]),
            notices: Vec::new(),
        });

        let stored = serde_json::to_string(&output).expect("a browse serializes");
        let restored: ToolOutput = serde_json::from_str(&stored).expect("and loads back");

        assert_eq!(restored.as_text(), output.as_text());
        assert_eq!(restored.annotation(), output.annotation());
    }

    /// A remote note has no path to name, so the reference and revision that
    /// reach it take that place and must survive into the text.
    #[test]
    fn a_remote_note_carries_the_locator_that_reaches_it() {
        const REFERENCE: &str = "memory-aaaa";
        const REVISION: &str = "bbbb";
        let output = ToolOutput::Memory(MemoryOutput::Notes {
            directory: None,
            notes: Vec::from([MemoryNote {
                origin: MemoryOrigin::Document {
                    reference: REFERENCE.into(),
                    revision: REVISION.into(),
                },
                ..memory_note("a.md", 1)
            }]),
            notices: Vec::new(),
        });

        assert_eq!(
            output.as_text(),
            format!(
                "a.md ({}) [{MEMORY_TAG}]\n\
                 {MEMORY_REFERENCE_LABEL}{REFERENCE}\n\
                 {MEMORY_REVISION_LABEL}{REVISION}\n\n{MEMORY_BODY}",
                token_label(1)
            )
        );
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
    #[test_case(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 1, lines: vec!["x".into(); 5], total_lines: 100, instructions: None }, Some("lines 1–5 of 100") ; "read_code_first_window")]
    #[test_case(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 10, lines: vec!["x".into(); 5], total_lines: 100, instructions: None }, Some("lines 10–14 of 100") ; "read_code_middle_window")]
    #[test_case(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 96, lines: vec!["x".into(); 5], total_lines: 100, instructions: None }, Some("lines 96–100 of 100") ; "read_code_window_reaches_eof")]
    #[test_case(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 101, lines: vec![], total_lines: 100, instructions: None }, Some("0 of 100 lines") ; "read_code_offset_past_eof")]
    #[test_case(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 1, lines: vec![], total_lines: 0, instructions: None }, Some("0 lines") ; "read_code_empty_file")]
    #[test_case(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 10, lines: vec!["x".into(); 5], total_lines: 0, instructions: None }, Some("5 lines") ; "read_code_old_session_without_total")]
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
                remote_written_paths: false,
                output_ref: None,
                output_limits: None,
                model_suffix: None,
                model_output: None,
                model_output_from_ref: false,
                accounting: ToolAccounting::default(),
            },
            ToolDoneEvent {
                id: "t2".into(),
                tool: Arc::from("read"),
                output: ToolOutput::Plain("fail".into()),
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
                accounting: ToolAccounting::default(),
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
                remote_written_paths: false,
                output_ref: None,
                output_limits: None,
                model_suffix: None,
                model_output: None,
                model_output_from_ref: false,
                accounting: ToolAccounting::default(),
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
            remote_written_paths: false,
            output_ref: Some(output_ref.clone()),
            output_limits: None,
            model_suffix: Some("model context".into()),
            model_output: Some("bounded preview\n\nmodel context".into()),
            model_output_from_ref: false,
            accounting: ToolAccounting::default(),
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
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            accounting: ToolAccounting::default(),
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
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            accounting: ToolAccounting::default(),
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
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            accounting: ToolAccounting::default(),
        };
        assert!(ok_event.wrote_to(Path::new("/plans/slug.md")));
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
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            accounting: ToolAccounting::default(),
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
    fn remote_written_path_never_completes_a_local_plan() {
        let plan = "/project/plan.md";
        let event = ToolDoneEvent {
            id: "call".into(),
            tool: Arc::from("file_write"),
            output: ToolOutput::Plain("written remotely".into()),
            is_error: false,
            annotation: None,
            written_path: Some(plan.into()),
            written_paths: vec![plan.into()],
            remote_written_paths: true,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            accounting: ToolAccounting::default(),
        };
        assert_eq!(event.written_path(), Some(plan));
        assert!(!event.wrote_to(Path::new(plan)));
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
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            accounting: ToolAccounting::default(),
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
    #[test_case(AgentEvent::Compacting, Some((COMPACTING_LABEL, None)) ; "compacting")]
    #[test_case(
        AgentEvent::Retry { attempt: 1, message: "overloaded".into(), delay_ms: 10 },
        Some((RETRYING_LABEL, None))
        ; "retrying"
    )]
    #[test_case(AgentEvent::CompactionDone, None ; "unrelated_event_leaves_it_unchanged")]
    #[test_case(AgentEvent::Nudge { attempt: 1, limit: 3 }, None ; "nudge_leaves_it_unchanged")]
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

    const STREAMED_THOUGHT: &str = "**Weighing it up**\n\nBoth read the same file.";

    #[test_case("", None, "" ; "nothing_is_revealed_yet")]
    #[test_case("*", None, "" ; "the_opening_fence_is_never_body")]
    #[test_case("**", None, "" ; "a_whole_opening_fence_is_silent_too")]
    #[test_case("**Weighing", None, "" ; "an_unfinished_title_waits_for_its_fence")]
    #[test_case("**Weighing it up*", None, "" ; "a_half_closed_fence_waits_as_well")]
    #[test_case("**Weighing it up**", Some("Weighing it up"), "" ; "the_closing_fence_names_the_header")]
    #[test_case("**Weighing it up**\n", Some("Weighing it up"), "" ; "a_half_separator_stays_out_of_the_body")]
    #[test_case(
        "**Weighing it up**\n\nBoth read",
        Some("Weighing it up"),
        "Both read"
        ; "the_body_follows_its_separator"
    )]
    fn a_streamed_thought_withholds_a_title_it_has_not_closed(
        visible: &str,
        title: Option<&str>,
        body: &str,
    ) {
        let summary = streaming_reasoning_summary(visible, STREAMED_THOUGHT);
        assert_eq!(summary.title, title);
        assert_eq!(summary.body, body);
    }

    #[test_case("Both read the same file.", "Both read the same file." ; "plain_prose")]
    #[test_case("**a**b", "**a**b**\n\nbody" ; "a_stray_star_disqualifies_the_buffer")]
    #[test_case("****", "****\n\nbody" ; "an_empty_title_disqualifies_it_too")]
    #[test_case(
        "**Important:**",
        "**Important:** keep this in the body."
        ; "the_buffer_disqualifies_what_the_reveal_still_shows_closed"
    )]
    fn a_thought_that_broke_the_title_shape_streams_as_body(visible: &str, buffered: &str) {
        let summary = streaming_reasoning_summary(visible, buffered);
        assert_eq!(summary.title, None);
        assert_eq!(summary.body, visible);
    }

    const WORKFLOW_RUN_ID: &str = "wf-run-1";
    const WORKFLOW_EPOCH: u64 = 3;
    const WORKFLOW_CALL_KEY: u64 = 42;
    const WORKFLOW_PHASE: &str = "review";
    const OTHER_WORKFLOW_RUN_ID: &str = "wf-run-2";

    fn provenance() -> WorkflowProvenance {
        WorkflowProvenance {
            run_id: WORKFLOW_RUN_ID.into(),
            epoch: WORKFLOW_EPOCH,
            call_key: WORKFLOW_CALL_KEY,
            phase: Some(WORKFLOW_PHASE.into()),
        }
    }

    fn workflow_sender() -> (EventSender, flume::Receiver<Envelope>) {
        let (tx, rx) = flume::unbounded();
        (EventSender::new(tx, 1).with_workflow(provenance()), rx)
    }

    #[test]
    fn a_workflow_sender_stamps_every_event_it_sends() {
        let (sender, rx) = workflow_sender();

        sender.send(AgentEvent::AuthRequired).unwrap();
        sender.try_send(AgentEvent::AuthRequired);

        let stamped: Vec<Envelope> = rx.drain().collect();
        assert_eq!(stamped.len(), 2);
        assert!(
            stamped
                .iter()
                .all(|envelope| envelope.workflow.as_ref() == Some(&provenance()))
        );
    }

    #[test]
    fn a_workflow_sender_fills_in_an_unstamped_envelope_but_keeps_a_stamped_one() {
        let (sender, rx) = workflow_sender();
        let foreign = WorkflowProvenance {
            run_id: OTHER_WORKFLOW_RUN_ID.into(),
            ..provenance()
        };

        sender
            .send_envelope(Envelope {
                event: AgentEvent::AuthRequired,
                subagent: None,
                run_id: 1,
                workflow: None,
            })
            .unwrap();
        sender
            .send_envelope(Envelope {
                event: AgentEvent::AuthRequired,
                subagent: None,
                run_id: 1,
                workflow: Some(foreign.clone()),
            })
            .unwrap();

        let stamped: Vec<Option<WorkflowProvenance>> =
            rx.drain().map(|envelope| envelope.workflow).collect();
        assert_eq!(stamped, [Some(provenance()), Some(foreign)]);
    }

    #[test]
    fn a_rebound_sender_keeps_its_run_and_provenance() {
        let (sender, _rx) = workflow_sender();
        let (tx, rx) = flume::unbounded();

        sender.rebind(tx).send(AgentEvent::AuthRequired).unwrap();

        let envelope = rx.try_recv().unwrap();
        assert_eq!(envelope.run_id, sender.run_id());
        assert_eq!(envelope.workflow.as_ref(), Some(&provenance()));
    }

    #[test]
    fn provenance_serializes_flattened_under_workflow_keys() {
        let (sender, rx) = workflow_sender();
        sender.send(AgentEvent::AuthRequired).unwrap();

        let json = serde_json::to_value(rx.try_recv().unwrap()).unwrap();

        assert_eq!(json["workflow_run_id"], WORKFLOW_RUN_ID);
        assert_eq!(json["workflow_epoch"], WORKFLOW_EPOCH);
        assert_eq!(json["workflow_call_key"], WORKFLOW_CALL_KEY);
        assert_eq!(json["workflow_phase"], WORKFLOW_PHASE);
        assert_eq!(json["run_id"], 1);
    }

    #[test]
    fn an_envelope_without_provenance_serializes_without_workflow_keys() {
        let (tx, rx) = flume::unbounded();
        EventSender::new(tx, 1)
            .send(AgentEvent::AuthRequired)
            .unwrap();

        let json = serde_json::to_value(rx.try_recv().unwrap()).unwrap();

        let keys: Vec<&String> = json
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| key.starts_with("workflow_"))
            .collect();
        assert!(keys.is_empty(), "unexpected keys: {keys:?}");
    }
}
