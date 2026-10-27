use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol_schema::v1::{
    CompactionId, CompactionStatus, CompactionUpdate, Content, ContentBlock, ContentChunk, Cost,
    Diff, ImageContent, Meta, Notice, NoticeSeverity, Plan, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, PromptResponse, SessionInfoUpdate, SessionUpdate, StopReason, TextContent,
    ToolCall, ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, Usage, UsageUpdate,
};
use caudra_agent::tools::ToolRegistry;
use caudra_agent::types::{
    TodoItem, TodoPriority, TodoStatus, ToolDoneEvent, ToolOutput, ToolStartEvent,
    TurnCompleteEvent,
};
use caudra_agent::{DoneReason, History};
use caudra_providers::{
    CaudraId, ContentBlock as MsgBlock, ImageMediaType, Message, Role as MsgRole, TokenUsage,
    add_cost,
};
use serde_json::{Value, json};

use crate::turn::{META_USAGE, PromptIdentity};

/// Where Caudra's own fields live in an ACP `_meta`.
pub(crate) const META_NAMESPACE: &str = "caudra";
pub(crate) const META_RETRY: &str = "retry";
pub(crate) const META_ATTEMPT: &str = "attempt";
pub(crate) const META_DELAY_MS: &str = "delayMs";
pub(crate) const RETRY_TITLE: &str = "Retrying request, attempt";
/// The tool a call ran, which the title and kind only describe.
pub(crate) const META_TOOL_NAME: &str = "toolName";
/// What a call still open when its turn ended reports.
pub(crate) const CANCELLED_TOOL_TEXT: &str = "Cancelled";
/// A thinking block's id is its message's id, this, and its position.
const THOUGHT_ID_INFIX: &str = ":thought:";
const RAW_STDOUT: &str = "stdout";
const RAW_STDERR: &str = "stderr";
const RAW_EXIT_CODE: &str = "exitCode";
const MIN_FENCE_LEN: usize = 3;
/// Model pricing is quoted in US dollars, so that is the reported currency.
const CURRENCY: &str = "USD";
/// `_meta` keys mirroring the ACP `Usage` field names, so a client reading
/// either source sees the same spelling and the same numbers.
const META_INPUT_TOKENS: &str = "inputTokens";
const META_OUTPUT_TOKENS: &str = "outputTokens";
const META_TOTAL_TOKENS: &str = "totalTokens";
const META_CACHED_READ_TOKENS: &str = "cachedReadTokens";
const META_CACHED_WRITE_TOKENS: &str = "cachedWriteTokens";
/// Cost has no home in ACP's `Usage`, and it is the number a spend cap needs.
const META_COST_USD: &str = "costUsd";

/// File-level tools report a location so the client can follow along. Directory
/// scoped tools (glob, grep, list) and commands (bash) target no single file.
const FILE_TOOLS: &[&str] = &[
    "read",
    "write",
    "edit",
    "multiedit",
    "edit_lines",
    "insert_lines",
    "file_edit",
    "file_read",
    "file_write",
    "file_index",
    "view_image",
];

/// Zed renders tool output as markdown, so bare text loses its newlines.
/// We wrap it in a backtick fence (longer than any run inside the text)
/// to keep the original formatting.
fn fenced(text: &str) -> String {
    let longest_backtick_run = text
        .split(|c: char| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(MIN_FENCE_LEN.max(longest_backtick_run + 1));
    format!("{fence}\n{text}\n{fence}")
}

pub fn tool_kind(registry: &ToolRegistry, name: &str) -> ToolKind {
    let entry = match registry.get(name) {
        Some(e) => e,
        None => return ToolKind::Other,
    };
    entry
        .tool
        .tool_kind()
        .map(parse_tool_kind)
        .unwrap_or(ToolKind::Other)
}

fn parse_tool_kind(s: &str) -> ToolKind {
    match s {
        "read" => ToolKind::Read,
        "edit" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        "move" => ToolKind::Move,
        "search" => ToolKind::Search,
        "execute" => ToolKind::Execute,
        "think" => ToolKind::Think,
        "fetch" => ToolKind::Fetch,
        "switch_mode" => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

/// Tool calls the client was told about and that have not finished, by id,
/// with the tool each runs.
pub type OpenCalls = HashMap<String, Arc<str>>;

/// Names the chunks of one assistant message. Text carries the message's own
/// id, and each run of thinking `<id>:thought:<n>`, so a live stream and a
/// replay of the stored message number them alike.
pub struct MessageCursor {
    id: String,
    thoughts: usize,
    thinking: bool,
}

impl MessageCursor {
    pub fn new(id: String) -> Self {
        Self {
            id,
            thoughts: 0,
            thinking: false,
        }
    }

    pub fn text(&mut self, text: &str) -> SessionUpdate {
        self.thinking = false;
        SessionUpdate::AgentMessageChunk(chunk(
            ContentBlock::Text(TextContent::new(text)),
            &self.id,
        ))
    }

    pub fn thought(&mut self, text: &str) -> SessionUpdate {
        if !self.thinking {
            self.thinking = true;
            self.thoughts += 1;
        }
        let id = format!("{}{THOUGHT_ID_INFIX}{}", self.id, self.thoughts - 1);
        SessionUpdate::AgentThoughtChunk(chunk(ContentBlock::Text(TextContent::new(text)), &id))
    }

    /// Something other than thinking came between, so the next thought is a
    /// new block.
    pub fn interrupt(&mut self) {
        self.thinking = false;
    }
}

fn chunk(content: ContentBlock, message_id: &str) -> ContentChunk {
    ContentChunk::new(content).message_id(message_id)
}

fn tool_meta(name: &str) -> Meta {
    Meta::from_iter([(META_NAMESPACE.to_owned(), json!({ META_TOOL_NAME: name }))])
}

fn call_update(id: &str, name: Option<&str>, fields: ToolCallUpdateFields) -> SessionUpdate {
    let update = ToolCallUpdate::new(ToolCallId::from(id.to_owned()), fields);
    SessionUpdate::ToolCallUpdate(match name {
        Some(name) => update.meta(tool_meta(name)),
        None => update,
    })
}

/// ACP treats `tool_call` as the creation and `tool_call_update` as a change to
/// one that exists, so a call is announced exactly once. Announcement waits for
/// the arguments to reveal a title, because a card labelled with the bare tool
/// name tells the user nothing, and clients that read the creation alone would
/// never learn what the call actually does.
///
/// The call is still `Pending`: only its title has firmed up.
pub fn tool_preview(
    registry: &ToolRegistry,
    id: &str,
    name: &str,
    preview: String,
    first: bool,
) -> SessionUpdate {
    if !first {
        return call_update(id, Some(name), ToolCallUpdateFields::new().title(preview));
    }
    SessionUpdate::ToolCall(
        ToolCall::new(ToolCallId::from(id.to_string()), preview)
            .kind(tool_kind(registry, name))
            .status(ToolCallStatus::Pending)
            .meta(tool_meta(name)),
    )
}

pub fn tool_start(
    registry: &ToolRegistry,
    event: &ToolStartEvent,
    cwd: &Path,
    home: Option<&Path>,
    first: bool,
) -> SessionUpdate {
    let locations = tool_locations(&event.tool, event.raw_input.as_ref(), cwd, home);
    let id = ToolCallId::from(event.id.clone());

    if first {
        let mut call = ToolCall::new(id, event.summary.clone())
            .kind(tool_kind(registry, &event.tool))
            .status(ToolCallStatus::InProgress)
            .locations(locations)
            .meta(tool_meta(&event.tool));
        if let Some(raw) = &event.raw_input {
            call = call.raw_input(raw.clone());
        }
        return SessionUpdate::ToolCall(call);
    }

    let mut fields = ToolCallUpdateFields::new()
        .status(ToolCallStatus::InProgress)
        .title(event.summary.clone());
    if let Some(raw) = &event.raw_input {
        fields = fields.raw_input(raw.clone());
    }
    if !locations.is_empty() {
        fields = fields.locations(locations);
    }
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, fields).meta(tool_meta(&event.tool)))
}

/// File locations the tool call touches, per ACP "Following the Agent". The
/// client expects absolute paths, so `~` and relative paths are resolved
/// against the session's home and cwd.
fn tool_locations(
    tool: &str,
    raw_input: Option<&serde_json::Value>,
    cwd: &Path,
    home: Option<&Path>,
) -> Vec<ToolCallLocation> {
    if !FILE_TOOLS.contains(&tool) {
        return Vec::new();
    }
    let Some(raw) = raw_input else {
        return Vec::new();
    };
    let Some(path) = input_path(raw) else {
        return Vec::new();
    };
    let Some(resolved) = resolve_path(path, cwd, home) else {
        return Vec::new();
    };
    vec![location(resolved, input_line(tool, raw))]
}

/// The target file under the native or legacy schema spelling.
fn input_path(raw_input: &serde_json::Value) -> Option<&str> {
    raw_input
        .get("path")
        .or_else(|| raw_input.get("file_path"))
        .or_else(|| raw_input.get("filePath"))?
        .as_str()
        .filter(|s| !s.is_empty())
}

/// `~`, `~/x`, and relative paths become absolute; other `~` spellings
/// (`~user`) have no expansion. ACP clients require absolute paths in
/// locations, so anything unresolvable yields None instead of a bogus path.
fn resolve_path(raw: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if raw.starts_with('~') {
        if raw == "~" {
            return home.map(Path::to_path_buf);
        }
        let rest = raw.strip_prefix("~/")?;
        return home.map(|h| h.join(rest));
    }
    let p = Path::new(raw);
    Some(if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    })
}

/// The line a tool call focuses on, when its input says so. ACP `line` is
/// 0-based (Zed uses it as a buffer row), but tool inputs are 1-based.
fn input_line(tool: &str, raw_input: &serde_json::Value) -> Option<u32> {
    let key = match tool {
        "read" | "file_read" => "offset",
        "edit_lines" => "start",
        // insert_lines writes *after* `line`, so the new text's 0-based row
        // is the raw value itself, and 0 (insert at the top) is valid.
        "insert_lines" => return raw_input.get("line").and_then(as_number),
        _ => return None,
    };
    raw_input.get(key).and_then(as_line).map(|l| l - 1)
}

fn as_line(v: &serde_json::Value) -> Option<u32> {
    as_number(v).filter(|&l| l >= 1)
}

/// raw_input is pre-validation, so models sometimes send numbers as strings.
fn as_number(v: &serde_json::Value) -> Option<u32> {
    let n = v
        .as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))?;
    u32::try_from(n).ok()
}

fn location(path: PathBuf, line: Option<u32>) -> ToolCallLocation {
    let mut loc = ToolCallLocation::new(path);
    if let Some(l) = line {
        loc = loc.line(l);
    }
    loc
}

/// A finished call's location comes from what it wrote. File tools already
/// reported that path at start, sometimes with a line, and an update replaces
/// locations instead of merging them, so re-reporting would drop the line the
/// client is following. Paths are already absolute (the plugins abspath them),
/// so resolve_path is a no-op here; it only guards against a relative slip.
fn done_locations(event: &ToolDoneEvent, cwd: &Path, home: Option<&Path>) -> Vec<ToolCallLocation> {
    if event.is_error || FILE_TOOLS.contains(&&*event.tool) {
        return Vec::new();
    }
    event
        .written_paths()
        .filter_map(|path| resolve_path(path, cwd, home))
        .map(|path| location(path, None))
        .collect()
}

pub fn tool_output(id: &str, name: Option<&str>, content: &str) -> SessionUpdate {
    let fields = ToolCallUpdateFields::new().content(vec![ToolCallContent::Content(Content::new(
        ContentBlock::Text(TextContent::new(fenced(content))),
    ))]);
    call_update(id, name, fields)
}

/// A call the turn ended or was cancelled under before it finished, which a
/// client would otherwise keep showing as running.
pub fn tool_cancelled(id: &str, name: &str) -> SessionUpdate {
    let fields = ToolCallUpdateFields::new()
        .status(ToolCallStatus::Failed)
        .content(vec![ToolCallContent::Content(Content::new(
            ContentBlock::Text(TextContent::new(CANCELLED_TOOL_TEXT)),
        ))]);
    call_update(id, Some(name), fields)
}

/// Settles every call in `open`, in id order.
pub fn cancel_open_calls(open: &mut OpenCalls) -> Vec<SessionUpdate> {
    let mut calls: Vec<_> = open.drain().collect();
    calls.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    calls
        .iter()
        .map(|(id, name)| tool_cancelled(id, name))
        .collect()
}

/// A shell's streams and exit code as the structured output a client reads,
/// any other output as the text the model read.
fn raw_output(output: &ToolOutput) -> Option<Value> {
    if let ToolOutput::Shell(shell) = output {
        return Some(json!({
            RAW_STDOUT: shell.stdout,
            RAW_STDERR: shell.stderr,
            RAW_EXIT_CODE: shell.exit_code,
        }));
    }
    let text = output.as_text();
    (!text.is_empty()).then_some(Value::String(text))
}

pub fn tool_done(event: &ToolDoneEvent, cwd: &Path, home: Option<&Path>) -> SessionUpdate {
    let status = if event.is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };

    let content = match &event.output {
        ToolOutput::Diff {
            path,
            before,
            after,
            ..
        } => {
            let diff = if before.is_empty() {
                Diff::new(path.as_str(), after.clone())
            } else {
                Diff::new(path.as_str(), after.clone()).old_text(before.clone())
            };
            vec![ToolCallContent::Diff(diff)]
        }
        _ => {
            let text = event.output.as_text();
            if text.is_empty() {
                vec![]
            } else {
                vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
                    TextContent::new(fenced(&text)),
                )))]
            }
        }
    };

    let mut fields = ToolCallUpdateFields::new().status(status).content(content);
    if let Some(raw) = raw_output(&event.output) {
        fields = fields.raw_output(raw);
    }

    let locations = done_locations(event, cwd, home);
    if !locations.is_empty() {
        fields = fields.locations(locations);
    }

    call_update(&event.id, Some(&event.tool), fields)
}

pub fn map_done_reason(reason: DoneReason) -> StopReason {
    match reason {
        DoneReason::EndTurn => StopReason::EndTurn,
        DoneReason::MaxTokens => StopReason::MaxTokens,
        DoneReason::MaxTurns => StopReason::MaxTurnRequests,
        DoneReason::Cancelled => StopReason::Cancelled,
        DoneReason::Refusal => StopReason::Refusal,
    }
}

/// One prompt spans however many provider calls the agent loop needs, and each
/// reports its own `TurnComplete`. The client bills the prompt, so the shares
/// accumulate here until the turn ends.
#[derive(Default)]
pub struct TurnSpend {
    usage: TokenUsage,
    cost: Option<f64>,
}

impl TurnSpend {
    pub fn add(&mut self, event: &TurnCompleteEvent) {
        self.usage += event.usage;
        add_cost(&mut self.cost, event.cost);
    }

    /// Reports the turn both ways on purpose: `usage` is the ACP field, but it
    /// rides an unstable feature the spec may change or drop, while `_meta` is
    /// stable extensibility any client can read. Both carry the same numbers,
    /// which `_meta.caudra` repeats beside the prompt's id and duration.
    pub(crate) fn into_response(self, stop: StopReason, prompt: &PromptIdentity) -> PromptResponse {
        let input = u64::from(self.usage.input);
        let output = u64::from(self.usage.output);
        let cached_read = u64::from(self.usage.cache_read);
        let cached_write = u64::from(self.usage.cache_creation);
        let total = input + output + cached_read + cached_write;

        let mut meta = Meta::new();
        meta.insert(META_INPUT_TOKENS.to_string(), input.into());
        meta.insert(META_OUTPUT_TOKENS.to_string(), output.into());
        meta.insert(META_TOTAL_TOKENS.to_string(), total.into());
        meta.insert(META_CACHED_READ_TOKENS.to_string(), cached_read.into());
        meta.insert(META_CACHED_WRITE_TOKENS.to_string(), cached_write.into());
        if let Some(cost) = self.cost {
            meta.insert(META_COST_USD.to_string(), cost.into());
        }
        let mut caudra = prompt.answer_meta();
        caudra.insert(META_USAGE.to_owned(), Value::Object(meta.clone()));
        meta.insert(META_NAMESPACE.to_owned(), Value::Object(caudra));

        let usage = Usage::new(total, input, output)
            .cached_read_tokens(cached_read)
            .cached_write_tokens(cached_write);

        PromptResponse::new(stop).usage(usage).meta(meta)
    }
}

/// The agent's todo list is what ACP calls a plan. `Cancelled` has no ACP
/// spelling, and reporting it as completed would claim work that never
/// happened, so those entries are dropped rather than misreported.
pub fn plan_update(output: &ToolOutput) -> Option<SessionUpdate> {
    let ToolOutput::TodoList(items) = output else {
        return None;
    };
    Some(plan(items))
}

/// The plan `items` spell, by the rules of [`plan_update`].
pub(crate) fn plan(items: &[TodoItem]) -> SessionUpdate {
    let entries = items
        .iter()
        .filter_map(|item| {
            let status = match item.status {
                TodoStatus::Pending => PlanEntryStatus::Pending,
                TodoStatus::InProgress => PlanEntryStatus::InProgress,
                TodoStatus::Completed => PlanEntryStatus::Completed,
                TodoStatus::Cancelled => return None,
            };
            let priority = match item.priority {
                TodoPriority::High => PlanEntryPriority::High,
                TodoPriority::Medium => PlanEntryPriority::Medium,
                TodoPriority::Low => PlanEntryPriority::Low,
            };
            Some(PlanEntry::new(item.content.clone(), priority, status))
        })
        .collect();
    SessionUpdate::Plan(Plan::new(entries))
}

/// Per ACP "Session Usage Updates": the current context gauge plus the
/// session's cumulative cost. Each turn's event only carries its own turn's
/// share, so the caller tracks the running total across turns.
pub fn usage_update(event: &TurnCompleteEvent, cost_total: Option<f64>) -> SessionUpdate {
    let used = event
        .context_size
        .unwrap_or_else(|| event.usage.context_tokens());
    context_usage(used, event.context_window, cost_total)
}

/// The gauge and spend as they stand, `used` of a `size` token window.
pub fn context_usage(used: u32, size: u32, cost_total: Option<f64>) -> SessionUpdate {
    let mut update = UsageUpdate::new(u64::from(used), u64::from(size));
    if let Some(cost) = cost_total {
        update = update.cost(Cost::new(cost, CURRENCY));
    }
    SessionUpdate::UsageUpdate(update)
}

/// The session's name, and when the session last changed when that is known.
pub fn session_info(title: String, updated_at: Option<String>) -> SessionUpdate {
    let mut update = SessionInfoUpdate::new().title(title);
    if let Some(updated_at) = updated_at {
        update = update.updated_at(updated_at);
    }
    SessionUpdate::SessionInfoUpdate(update)
}

/// Where compaction `id` stands; `error` says why a failed one failed.
pub fn compaction_update(
    id: &CompactionId,
    status: CompactionStatus,
    error: Option<String>,
) -> SessionUpdate {
    let mut update = CompactionUpdate::new(id.clone(), status);
    if let Some(error) = error {
        update = update.error(error);
    }
    SessionUpdate::CompactionUpdate(update)
}

/// How a compaction ended, from how the run that carried it did.
pub fn compaction_outcome(reason: &DoneReason) -> CompactionStatus {
    match reason {
        DoneReason::Cancelled => CompactionStatus::Cancelled,
        DoneReason::EndTurn
        | DoneReason::MaxTokens
        | DoneReason::MaxTurns
        | DoneReason::Refusal => CompactionStatus::Completed,
    }
}

/// A provider request failed and is retried after `delay_ms`.
pub fn retry_notice(attempt: u32, message: String, delay_ms: u64) -> SessionUpdate {
    let meta = Meta::from_iter([(
        META_NAMESPACE.to_owned(),
        json!({ META_RETRY: { META_ATTEMPT: attempt, META_DELAY_MS: delay_ms } }),
    )]);
    let notice = Notice::new(NoticeSeverity::Warning, format!("{RETRY_TITLE} {attempt}"))
        .description(message)
        .meta(meta);
    SessionUpdate::Notice(notice)
}

/// The id each message of `history` was committed under: the group its
/// items share, which is also the id its live chunks carried. A history whose
/// items do not line up with its messages has none.
fn message_ids(history: &History) -> Option<Vec<CaudraId>> {
    let ids: Vec<_> = history
        .active_items()
        .chunk_by(|a, b| a.group_id == b.group_id)
        .map(|group| group[0].group_id)
        .collect();
    (ids.len() == history.len()).then_some(ids)
}

pub fn replay_history(
    registry: &ToolRegistry,
    history: &History,
    cwd: &Path,
    home: Option<&Path>,
) -> Vec<SessionUpdate> {
    let ids = message_ids(history);
    let mut replay = Replay {
        registry,
        cwd,
        home,
        names: HashMap::new(),
        updates: Vec::new(),
    };
    for (index, msg) in history.as_slice().iter().enumerate() {
        let id = ids
            .as_ref()
            .map_or_else(CaudraId::generate, |ids| ids[index])
            .to_string();
        match msg.role {
            MsgRole::User => replay.user(msg, &id),
            MsgRole::Assistant => replay.assistant(msg, id),
        }
    }
    replay.updates
}

struct Replay<'a> {
    registry: &'a ToolRegistry,
    cwd: &'a Path,
    home: Option<&'a Path>,
    /// The tool each replayed call ran, which its result names again.
    names: HashMap<&'a str, &'a str>,
    updates: Vec<SessionUpdate>,
}

impl<'a> Replay<'a> {
    fn user(&mut self, msg: &'a Message, id: &str) {
        if msg.is_observation() {
            return;
        }
        if let Some(text) = msg.user_text() {
            self.updates.push(SessionUpdate::UserMessageChunk(chunk(
                ContentBlock::Text(TextContent::new(text)),
                id,
            )));
        }
        for block in &msg.content {
            match block {
                MsgBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => {
                    let name = self.names.get(tool_use_id.as_str()).copied();
                    self.updates
                        .push(replay_tool_result(tool_use_id, name, content, *is_error));
                }
                MsgBlock::Image { source } => {
                    self.updates.push(SessionUpdate::UserMessageChunk(chunk(
                        ContentBlock::Image(ImageContent::new(
                            source.data.to_string(),
                            mime_type(&source.media_type),
                        )),
                        id,
                    )));
                }
                _ => {}
            }
        }
    }

    fn assistant(&mut self, msg: &'a Message, id: String) {
        let mut cursor = MessageCursor::new(id);
        for block in &msg.content {
            match block {
                MsgBlock::Text { text } => self.updates.push(cursor.text(text)),
                MsgBlock::Thinking { thinking, .. } => self.updates.push(cursor.thought(thinking)),
                MsgBlock::ToolUse {
                    id, name, input, ..
                } => {
                    cursor.interrupt();
                    self.names.insert(id, name);
                    self.updates.push(replay_tool_call(
                        self.registry,
                        id,
                        name,
                        input,
                        self.cwd,
                        self.home,
                    ));
                }
                _ => cursor.interrupt(),
            }
        }
    }
}

fn replay_tool_call(
    registry: &ToolRegistry,
    id: &str,
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
    home: Option<&Path>,
) -> SessionUpdate {
    let call = ToolCall::new(ToolCallId::from(id.to_string()), name.to_string())
        .kind(tool_kind(registry, name))
        .status(ToolCallStatus::Pending)
        .raw_input(input.clone())
        .locations(tool_locations(name, Some(input), cwd, home))
        .meta(tool_meta(name));
    SessionUpdate::ToolCall(call)
}

fn replay_tool_result(
    id: &str,
    name: Option<&str>,
    content: &str,
    is_error: bool,
) -> SessionUpdate {
    let status = if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };
    let mut fields = ToolCallUpdateFields::new().status(status);
    if !content.is_empty() {
        fields = fields.content(vec![ToolCallContent::Content(Content::new(
            ContentBlock::Text(TextContent::new(fenced(content))),
        ))]);
    }
    call_update(id, name, fields)
}

fn mime_type(media: &ImageMediaType) -> &'static str {
    match media {
        ImageMediaType::Png => "image/png",
        ImageMediaType::Jpeg => "image/jpeg",
        ImageMediaType::Gif => "image/gif",
        ImageMediaType::Webp => "image/webp",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use caudra_agent::tools::{VIEW_IMAGE_TOOL_NAME, native};
    use caudra_agent::types::{ShellOutput, TodoItem};
    use caudra_config::FeatureFlags;
    use caudra_providers::{ImageSource, expand_message_in};
    use caudra_storage::usage_ledger::LedgerPurpose;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const CWD: &str = "/home/user/project";
    const HOME: &str = "/home/user";
    const OMITTED_STATUS_IS_PENDING: &str = "a created call must be pending, which the wire omits";
    const SHELL_TOOL: &str = "shell";
    const SHELL_STDOUT: &str = "compiled";
    const SHELL_STDERR: &str = "warning: unused";
    const SHELL_EXIT: i32 = 101;
    const SHELL_MODEL_TEXT: &str = "compiled\n[shell status: exit code 101]";

    #[test_case("1: mod render\n2: mod segment", "```\n1: mod render\n2: mod segment\n```" ; "plain_text_gets_default_fence")]
    #[test_case("has ```rust\ncode\n``` inside", "````\nhas ```rust\ncode\n``` inside\n````" ; "fence_longer_than_inner_backticks")]
    fn fenced_wraps_in_code_block(input: &str, expected: &str) {
        assert_eq!(fenced(input), expected);
    }

    /// The only pair whose names disagree, and the one ACP clients read to tell
    /// "the model stopped" from "the agent ran out of turns".
    #[test]
    fn max_turns_maps_to_max_turn_requests() {
        assert_eq!(
            map_done_reason(DoneReason::MaxTurns),
            StopReason::MaxTurnRequests
        );
    }

    fn assistant(content: Vec<MsgBlock>) -> Message {
        Message {
            role: MsgRole::Assistant,
            content,
            display_text: None,
            ..Default::default()
        }
    }

    fn updates_json(messages: &[Message]) -> Vec<serde_json::Value> {
        replay_history(
            &ToolRegistry::default(),
            &History::new(messages.to_vec()),
            Path::new(CWD),
            Some(Path::new(HOME)),
        )
        .iter()
        .map(|u| serde_json::to_value(u).unwrap())
        .collect()
    }

    #[test]
    fn replay_full_conversation_in_order() {
        let messages = vec![
            Message::user("hello".into()),
            assistant(vec![
                MsgBlock::thinking("hmm".into(), None),
                MsgBlock::Text {
                    text: "let me check".into(),
                },
                MsgBlock::tool_use("tu-1", "bash", serde_json::json!({"command": "ls"})),
            ]),
            Message {
                role: MsgRole::User,
                content: vec![MsgBlock::ToolResult {
                    tool_use_id: "tu-1".into(),
                    content: "file.rs".into(),
                    is_error: false,
                    output_ref: None,
                }],
                display_text: None,
                ..Default::default()
            },
            assistant(vec![MsgBlock::Text {
                text: "done".into(),
            }]),
        ];

        let json = updates_json(&messages);
        assert_eq!(json.len(), 6);
        assert_eq!(json[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(json[0]["content"]["text"], "hello");
        assert_eq!(json[1]["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(json[1]["content"]["text"], "hmm");
        assert_eq!(json[2]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(json[2]["content"]["text"], "let me check");
        assert_eq!(json[3]["sessionUpdate"], "tool_call");
        assert_eq!(json[3]["toolCallId"], "tu-1");
        assert!(json[3]["kind"].is_null());
        assert_eq!(json[3]["rawInput"]["command"], "ls");
        assert_eq!(json[4]["sessionUpdate"], "tool_call_update");
        assert_eq!(json[4]["toolCallId"], "tu-1");
        assert_eq!(json[4]["status"], "completed");
        assert_eq!(
            json[4]["content"][0]["content"]["text"],
            "```\nfile.rs\n```"
        );
        assert_eq!(json[5]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(json[5]["content"]["text"], "done");
    }

    #[test]
    fn replay_prefers_display_text_over_model_text() {
        let msg = Message::user_display("expanded with context".into(), "what user typed".into());
        let json = updates_json(&[msg]);
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["content"]["text"], "what user typed");
    }

    #[test]
    fn replay_hides_synthetic_messages() {
        assert!(updates_json(&[Message::synthetic("injected".into())]).is_empty());
    }

    #[test]
    fn replay_never_speaks_an_observation_as_the_user() {
        let obs = Message::observation("[monitor] build failed".into());
        assert!(updates_json(&[obs]).is_empty());
    }

    #[test]
    fn replay_failed_tool_result_maps_to_failed_status() {
        let msg = Message {
            role: MsgRole::User,
            content: vec![MsgBlock::ToolResult {
                tool_use_id: "tu-err".into(),
                content: "boom".into(),
                is_error: true,
                output_ref: None,
            }],
            display_text: None,
            ..Default::default()
        };
        let json = updates_json(&[msg]);
        assert_eq!(json[0]["sessionUpdate"], "tool_call_update");
        assert_eq!(json[0]["status"], "failed");
    }

    #[test]
    fn replay_user_image_keeps_mime_type() {
        let msg = Message::user_with_images(
            String::new(),
            vec![ImageSource {
                media_type: ImageMediaType::Png,
                data: std::sync::Arc::from("b64data"),
            }],
        );
        let json = updates_json(&[msg]);
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(json[0]["content"]["type"], "image");
        assert_eq!(json[0]["content"]["mimeType"], "image/png");
        assert_eq!(json[0]["content"]["data"], "b64data");
    }

    #[test_case("read", ToolKind::Read ; "read")]
    #[test_case("edit", ToolKind::Edit ; "edit")]
    #[test_case("delete", ToolKind::Delete ; "delete")]
    #[test_case("move", ToolKind::Move ; "move_kind")]
    #[test_case("search", ToolKind::Search ; "search")]
    #[test_case("execute", ToolKind::Execute ; "execute")]
    #[test_case("think", ToolKind::Think ; "think")]
    #[test_case("fetch", ToolKind::Fetch ; "fetch")]
    #[test_case("switch_mode", ToolKind::SwitchMode ; "switch_mode")]
    #[test_case("other", ToolKind::Other ; "other")]
    #[test_case("bogus", ToolKind::Other ; "unknown_maps_to_other")]
    fn parse_tool_kind_maps_wire_strings(input: &str, expected: ToolKind) {
        assert_eq!(parse_tool_kind(input), expected);
    }

    #[test_case(true, ToolKind::Read ; "registered_in_this_registry")]
    #[test_case(false, ToolKind::Other ; "registered_only_elsewhere")]
    fn tool_kind_reads_only_the_given_registry(own: bool, expected: ToolKind) {
        let registered = ToolRegistry::default();
        native::register(&registered, FeatureFlags::all()).unwrap();
        let other = ToolRegistry::default();
        let registry = if own { &registered } else { &other };
        assert_eq!(tool_kind(registry, VIEW_IMAGE_TOOL_NAME), expected);
    }

    fn start_event(tool: &str, raw_input: Option<serde_json::Value>) -> ToolStartEvent {
        ToolStartEvent {
            id: "t-1".into(),
            effect: caudra_agent::tools::ToolEffect::Unknown,
            tool: Arc::from(tool),
            summary: String::new(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input,
            output: None,
        }
    }

    fn start_locations(
        tool: &str,
        raw_input: Option<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        let update = tool_start(
            &ToolRegistry::default(),
            &start_event(tool, raw_input),
            Path::new(CWD),
            Some(Path::new(HOME)),
            false,
        );
        serde_json::to_value(update)
            .unwrap()
            .get("locations")
            .cloned()
    }

    #[test_case("read", Some(json!({"path": "/a/b.rs", "offset": 42, "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 41}])) ; "read_absolute_with_offset")]
    #[test_case("read", Some(json!({"path": "/a/b.rs", "offset": 1, "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 0}])) ; "offset_one_reports_line_zero")]
    #[test_case("read", Some(json!({"path": "src/lib.rs", "offset": 1, "limit": 0})), Some(json!([{"path": "/home/user/project/src/lib.rs", "line": 0}])) ; "read_relative_resolved_against_cwd")]
    #[test_case("read", Some(json!({"path": "~/notes.md", "offset": 1, "limit": 0})), Some(json!([{"path": "/home/user/notes.md", "line": 0}])) ; "read_tilde_expanded")]
    #[test_case("read", Some(json!({"path": "~", "offset": 1, "limit": 0})), Some(json!([{"path": "/home/user", "line": 0}])) ; "read_bare_tilde_expands_to_home")]
    #[test_case("read", Some(json!({"path": "~other/x", "offset": 1, "limit": 0})), None ; "read_tilde_user_prefix_unresolvable")]
    #[test_case("read", Some(json!({"file_path": "/a/b.rs", "offset": 7, "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 6}])) ; "read_file_path_alias")]
    #[test_case("file_read", Some(json!({"filePath": "/a/b.rs", "offset": 7, "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 6}])) ; "native_read_file_path")]
    #[test_case("read", Some(json!({"path": "/a/b.rs", "offset": "42", "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 41}])) ; "read_string_offset_coerced")]
    #[test_case("write", Some(json!({"path": "/a/b.rs", "content": "x"})), Some(json!([{"path": "/a/b.rs"}])) ; "write_no_line")]
    #[test_case("edit", Some(json!({"path": "c.rs", "old_string": "a", "new_string": "b"})), Some(json!([{"path": "/home/user/project/c.rs"}])) ; "edit_relative_no_line")]
    #[test_case("file_edit", Some(json!({"filePath": "c.rs", "oldString": "a", "newString": "b"})), Some(json!([{"path": "/home/user/project/c.rs"}])) ; "native_edit_relative")]
    #[test_case("file_write", Some(json!({"filePath": "/a/b.rs", "content": "x"})), Some(json!([{"path": "/a/b.rs"}])) ; "native_write")]
    #[test_case("multiedit", Some(json!({"path": "c.rs", "edits": []})), Some(json!([{"path": "/home/user/project/c.rs"}])) ; "multiedit_relative_no_line")]
    #[test_case("edit_lines", Some(json!({"path": "/a", "start": 3, "end": 9, "new_string": "n"})), Some(json!([{"path": "/a", "line": 2}])) ; "edit_lines_start_becomes_line")]
    #[test_case("insert_lines", Some(json!({"path": "/a", "line": 5, "new_string": "n"})), Some(json!([{"path": "/a", "line": 5}])) ; "insert_lines_reports_the_inserted_row")]
    #[test_case("insert_lines", Some(json!({"path": "/a", "line": 0, "new_string": "n"})), Some(json!([{"path": "/a", "line": 0}])) ; "insert_lines_at_top")]
    #[test_case("file_index", Some(json!({"path": "/a/b.rs"})), Some(json!([{"path": "/a/b.rs"}])) ; "index_file_path")]
    #[test_case("view_image", Some(json!({"path": "img.png"})), Some(json!([{"path": "/home/user/project/img.png"}])) ; "view_image_relative")]
    #[test_case("glob", Some(json!({"pattern": "*.rs", "path": "src"})), None ; "glob_directory_path_ignored")]
    #[test_case("grep", Some(json!({"pattern": "x", "path": "src"})), None ; "grep_directory_path_ignored")]
    #[test_case("bash", Some(json!({"command": "ls", "workdir": "/tmp"})), None ; "bash_workdir_ignored")]
    #[test_case("read", None, None ; "missing_input_no_locations")]
    #[test_case("read", Some(json!({"offset": 1, "limit": 0})), None ; "missing_path_no_locations")]
    #[test_case("read", Some(json!({"path": "", "offset": 1, "limit": 0})), None ; "empty_path_no_locations")]
    fn tool_start_locations(
        tool: &str,
        input: Option<serde_json::Value>,
        expected: Option<serde_json::Value>,
    ) {
        assert_eq!(start_locations(tool, input), expected);
    }

    #[test_case(true ; "on_creation")]
    #[test_case(false ; "on_update")]
    fn tool_start_with_no_raw_input_has_no_locations_field(first: bool) {
        let update = tool_start(
            &ToolRegistry::default(),
            &start_event("read", None),
            Path::new(CWD),
            Some(Path::new(HOME)),
            first,
        );
        let json = serde_json::to_value(update).unwrap();
        assert!(
            json.get("locations").is_none(),
            "empty locations must be omitted: {json}"
        );
    }

    /// A client that reads only the creation notification still learns what the
    /// call does, which is the whole point of holding the announcement back.
    #[test]
    fn first_tool_start_creates_the_call_with_title_and_raw_input() {
        let mut event = start_event("bash", Some(json!({"command": "ls -la"})));
        event.summary = "ls -la".into();
        let json = serde_json::to_value(tool_start(
            &ToolRegistry::default(),
            &event,
            Path::new(CWD),
            Some(Path::new(HOME)),
            true,
        ))
        .unwrap();

        assert_eq!(json["sessionUpdate"], json!("tool_call"));
        assert_eq!(json["title"], json!("ls -la"));
        assert_eq!(json["rawInput"], json!({"command": "ls -la"}));
        assert_eq!(json["status"], json!("in_progress"));
    }

    #[test]
    fn later_tool_start_updates_the_existing_call() {
        let json = serde_json::to_value(tool_start(
            &ToolRegistry::default(),
            &start_event("bash", None),
            Path::new(CWD),
            Some(Path::new(HOME)),
            false,
        ))
        .unwrap();

        assert_eq!(json["sessionUpdate"], json!("tool_call_update"));
    }

    #[test]
    fn first_preview_creates_a_pending_call_titled_by_the_preview() {
        let json = serde_json::to_value(tool_preview(
            &ToolRegistry::default(),
            "t-1",
            "bash",
            "cargo test".into(),
            true,
        ))
        .unwrap();

        assert_eq!(json["sessionUpdate"], json!("tool_call"));
        assert_eq!(json["title"], json!("cargo test"));
        // `pending` is the schema default, so the wire omits it.
        assert!(json.get("status").is_none(), "{OMITTED_STATUS_IS_PENDING}");
    }

    #[test]
    fn todo_list_becomes_a_plan_without_the_cancelled_entries() {
        let todo = |content: &str, status, priority| TodoItem {
            content: content.into(),
            status,
            priority,
        };
        let output = ToolOutput::TodoList(vec![
            todo("ship it", TodoStatus::InProgress, TodoPriority::High),
            todo("drop it", TodoStatus::Cancelled, TodoPriority::Low),
            todo("done it", TodoStatus::Completed, TodoPriority::Medium),
        ]);

        let json = serde_json::to_value(plan_update(&output).unwrap()).unwrap();

        assert_eq!(json["sessionUpdate"], json!("plan"));
        assert_eq!(
            json["entries"],
            json!([
                {"content": "ship it", "priority": "high", "status": "in_progress"},
                {"content": "done it", "priority": "medium", "status": "completed"},
            ])
        );
    }

    #[test]
    fn non_todo_output_is_not_a_plan() {
        assert!(plan_update(&ToolOutput::Plain("hello".into())).is_none());
    }

    #[test]
    fn later_preview_only_retitles() {
        let json = serde_json::to_value(tool_preview(
            &ToolRegistry::default(),
            "t-1",
            "bash",
            "cargo test".into(),
            false,
        ))
        .unwrap();

        assert_eq!(json["sessionUpdate"], json!("tool_call_update"));
        assert_eq!(json["title"], json!("cargo test"));
    }

    fn done_event(
        tool: &str,
        output: ToolOutput,
        is_error: bool,
        written: Option<&str>,
    ) -> ToolDoneEvent {
        ToolDoneEvent {
            id: "t-1".into(),
            tool: Arc::from(tool),
            output,
            is_error,
            annotation: None,
            written_path: written.map(str::to_owned),
            written_paths: Vec::new(),
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            documents: Vec::new(),
            accounting: caudra_agent::types::ToolAccounting::default(),
        }
    }

    fn done_locations_of(event: &ToolDoneEvent) -> Option<serde_json::Value> {
        let update = tool_done(event, Path::new(CWD), Some(Path::new(HOME)));
        serde_json::to_value(update)
            .unwrap()
            .get("locations")
            .cloned()
    }

    const OVERWRITTEN_PATH: &str = "/home/user/project/src/lib.rs";
    const NATIVE_DIFF_MSG: &str =
        "an editor gets a diff it can render, not a fenced blob it has to parse";

    /// Now that an overwrite settles into a diff, this branch carries every
    /// `file_write` that replaced something, not just a single-match edit.
    #[test]
    fn a_two_sided_edit_crosses_as_a_native_diff() {
        let event = done_event(
            "file_write",
            ToolOutput::Diff {
                path: OVERWRITTEN_PATH.into(),
                before: "fn old() {}\n".into(),
                after: "fn new() {}\n".into(),
                summary: String::new(),
            },
            false,
            Some(OVERWRITTEN_PATH),
        );
        let json = serde_json::to_value(tool_done(&event, Path::new(CWD), Some(Path::new(HOME))))
            .expect("serialize");
        assert_eq!(
            json["content"],
            json!([{
                "type": "diff",
                "path": OVERWRITTEN_PATH,
                "oldText": "fn old() {}\n",
                "newText": "fn new() {}\n",
            }]),
            "{NATIVE_DIFF_MSG}: {json}"
        );
    }

    #[test]
    fn done_written_path_reports_location_without_line() {
        let event = done_event(
            "memory",
            ToolOutput::Plain("wrote 3 bytes".into()),
            false,
            Some("/home/user/project/a.rs"),
        );
        assert_eq!(
            done_locations_of(&event),
            Some(json!([{"path": "/home/user/project/a.rs"}]))
        );
    }

    #[test]
    fn done_plural_written_paths_report_every_location() {
        let mut event = done_event(
            "patch",
            ToolOutput::Plain("patched files".into()),
            false,
            Some("/home/user/project/a.rs"),
        );
        event.written_paths = vec![
            "/home/user/project/a.rs".into(),
            "/home/user/project/b.rs".into(),
        ];

        assert_eq!(
            done_locations_of(&event),
            Some(json!([
                {"path": "/home/user/project/a.rs"},
                {"path": "/home/user/project/b.rs"}
            ]))
        );
    }

    /// A file tool's start event already reported the path, with the line the
    /// client is following. Re-reporting it here would replace that line.
    #[test_case("edit_lines" ; "edit_lines_keeps_start_line")]
    #[test_case("insert_lines" ; "insert_lines_keeps_start_line")]
    #[test_case("write" ; "write_keeps_start_location")]
    fn done_file_tool_omits_written_path_location(tool: &str) {
        let event = done_event(
            tool,
            ToolOutput::Plain("wrote 3 bytes".into()),
            false,
            Some("/home/user/project/a.rs"),
        );
        assert_eq!(done_locations_of(&event), None);
    }

    #[test]
    fn done_error_suppresses_written_path_location() {
        let event = done_event(
            "memory",
            ToolOutput::Plain("write error: permission denied".into()),
            true,
            Some("/home/user/project/a.rs"),
        );
        assert_eq!(done_locations_of(&event), None);
    }

    #[test]
    fn done_without_file_output_has_no_locations() {
        let event = done_event("memory", ToolOutput::Plain("done".into()), false, None);
        assert_eq!(done_locations_of(&event), None);
    }

    #[test]
    fn replay_tool_use_reports_locations() {
        let msg = assistant(vec![MsgBlock::tool_use(
            "tu-1",
            "read",
            json!({"path": "src/lib.rs", "offset": 10, "limit": 0}),
        )]);
        let json = updates_json(&[msg]);
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["sessionUpdate"], "tool_call");
        assert_eq!(json[0]["toolCallId"], "tu-1");
        assert_eq!(
            json[0]["locations"],
            json!([{"path": "/home/user/project/src/lib.rs", "line": 9}])
        );
    }

    #[test]
    fn replay_non_file_tool_has_no_locations() {
        let msg = assistant(vec![MsgBlock::tool_use(
            "tu-1",
            "bash",
            json!({"command": "ls"}),
        )]);
        let json = updates_json(&[msg]);
        assert!(json[0].get("locations").is_none(), "{:?}", json[0]);
    }

    fn turn_event(
        context_size: Option<u32>,
        context_window: u32,
        cost: Option<f64>,
    ) -> TurnCompleteEvent {
        TurnCompleteEvent {
            interrupted: false,
            message: Message::default(),
            usage: caudra_providers::TokenUsage {
                input: 1_000,
                output: 200,
                cache_creation: 0,
                cache_read: 50_000,
            },
            model: "test-model".into(),
            provider: "test-provider".into(),
            purpose: LedgerPurpose::Chat,
            cost,
            billing: caudra_providers::Billing::Api,
            account: None,
            context_size,
            context_window,
        }
    }

    #[test]
    fn usage_update_reports_gauge_and_cumulative_cost() {
        let event = turn_event(Some(60_000), 200_000, Some(0.05));
        let json = serde_json::to_value(usage_update(&event, Some(0.125))).unwrap();
        assert_eq!(json["sessionUpdate"], "usage_update");
        assert_eq!(json["used"], 60_000);
        assert_eq!(json["size"], 200_000);
        assert_eq!(json["cost"]["amount"], 0.125);
        assert_eq!(json["cost"]["currency"], CURRENCY);
    }

    #[test]
    fn usage_update_without_cost_omits_cost() {
        let event = turn_event(Some(60_000), 200_000, None);
        let json = serde_json::to_value(usage_update(&event, None)).unwrap();
        assert_eq!(json["used"], 60_000);
        assert_eq!(json["size"], 200_000);
        assert!(json.get("cost").is_none(), "{json}");
    }

    #[test]
    fn usage_update_falls_back_to_usage_when_context_size_missing() {
        let event = turn_event(None, 200_000, None);
        let json = serde_json::to_value(usage_update(&event, None)).unwrap();
        assert_eq!(json["used"], 51_200);
    }

    fn tool_name_of(update: SessionUpdate) -> Value {
        serde_json::to_value(update).unwrap()["_meta"][META_NAMESPACE][META_TOOL_NAME].clone()
    }

    fn shell_output(exit_code: Option<i32>) -> ToolOutput {
        ToolOutput::Shell(ShellOutput {
            model_text: SHELL_MODEL_TEXT.into(),
            relative_workdir: ".".into(),
            timeout_ms: 0,
            duration_ms: 0,
            exit_code,
            signal: None,
            timed_out: false,
            output_limit_exceeded: false,
            final_sequence: 0,
            stdout_utf8_bytes: 0,
            stderr_utf8_bytes: 0,
            stdout: SHELL_STDOUT.into(),
            stderr: SHELL_STDERR.into(),
            stdout_capture_truncated: false,
            stderr_capture_truncated: false,
            stdout_preview_truncated: false,
            stderr_preview_truncated: false,
            stdout_redraws_collapsed: 0,
            stderr_redraws_collapsed: 0,
            filter: None,
        })
    }

    #[test_case(tool_preview(&ToolRegistry::default(), "t-1", SHELL_TOOL, "ls".into(), true) ; "preview_creation")]
    #[test_case(tool_preview(&ToolRegistry::default(), "t-1", SHELL_TOOL, "ls".into(), false) ; "preview_update")]
    #[test_case(tool_start(&ToolRegistry::default(), &start_event(SHELL_TOOL, None), Path::new(CWD), None, true) ; "start_creation")]
    #[test_case(tool_start(&ToolRegistry::default(), &start_event(SHELL_TOOL, None), Path::new(CWD), None, false) ; "start_update")]
    #[test_case(tool_output("t-1", Some(SHELL_TOOL), SHELL_STDOUT) ; "streamed_output")]
    #[test_case(tool_done(&done_event(SHELL_TOOL, shell_output(Some(0)), false, None), Path::new(CWD), None) ; "done")]
    #[test_case(tool_cancelled("t-1", SHELL_TOOL) ; "cancelled")]
    fn every_tool_update_names_its_tool(update: SessionUpdate) {
        assert_eq!(tool_name_of(update), SHELL_TOOL);
    }

    #[test]
    fn a_replayed_call_and_its_result_name_their_tool() {
        let json = updates_json(&[
            assistant(vec![MsgBlock::tool_use(
                "tu-1",
                SHELL_TOOL,
                json!({"command": "ls"}),
            )]),
            Message {
                role: MsgRole::User,
                content: vec![MsgBlock::ToolResult {
                    tool_use_id: "tu-1".into(),
                    content: SHELL_STDOUT.into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ]);
        for update in &json {
            assert_eq!(update["_meta"][META_NAMESPACE][META_TOOL_NAME], SHELL_TOOL);
        }
    }

    /// Orca reads a shell's streams and exit code from `rawOutput`, and the
    /// fenced text stays for clients that only render content.
    #[test_case(Some(SHELL_EXIT), json!(SHELL_EXIT) ; "exited")]
    #[test_case(None, Value::Null ; "killed_without_a_code")]
    fn a_shell_reports_its_streams_and_exit_code(exit_code: Option<i32>, expected: Value) {
        let event = done_event(SHELL_TOOL, shell_output(exit_code), true, None);
        let json = serde_json::to_value(tool_done(&event, Path::new(CWD), None)).unwrap();

        assert_eq!(
            json["rawOutput"],
            json!({RAW_STDOUT: SHELL_STDOUT, RAW_STDERR: SHELL_STDERR, RAW_EXIT_CODE: expected})
        );
        assert_eq!(
            json["content"][0]["content"]["text"],
            fenced(&event.output.as_text())
        );
    }

    #[test]
    fn other_output_stays_raw_text() {
        let event = done_event(
            "memory",
            ToolOutput::Plain(SHELL_STDOUT.into()),
            false,
            None,
        );
        let json = serde_json::to_value(tool_done(&event, Path::new(CWD), None)).unwrap();
        assert_eq!(json["rawOutput"], SHELL_STDOUT);
    }

    #[test]
    fn open_calls_fail_as_cancelled_in_id_order() {
        let mut open = OpenCalls::from([
            ("t-2".to_owned(), Arc::from(SHELL_TOOL)),
            ("t-1".to_owned(), Arc::from(VIEW_IMAGE_TOOL_NAME)),
        ]);

        let json: Vec<Value> = cancel_open_calls(&mut open)
            .into_iter()
            .map(|update| serde_json::to_value(update).unwrap())
            .collect();

        assert!(open.is_empty());
        let ids: Vec<&Value> = json.iter().map(|update| &update["toolCallId"]).collect();
        assert_eq!(ids, [&json!("t-1"), &json!("t-2")]);
        for update in &json {
            assert_eq!(update["status"], "failed");
            assert_eq!(update["content"][0]["content"]["text"], CANCELLED_TOOL_TEXT);
        }
    }

    #[test_case(DoneReason::EndTurn, StopReason::EndTurn ; "end_turn")]
    #[test_case(DoneReason::Refusal, StopReason::Refusal ; "refusal")]
    #[test_case(DoneReason::Cancelled, StopReason::Cancelled ; "cancelled")]
    fn done_reasons_map_to_stop_reasons(reason: DoneReason, expected: StopReason) {
        assert_eq!(map_done_reason(reason), expected);
    }

    /// The ids a replay gives, which must be the ones the stored groups carry.
    #[test]
    fn replayed_chunks_carry_their_stored_message_ids() {
        let user = CaudraId::generate();
        let reply = CaudraId::generate();
        let mut items = expand_message_in(&Message::user("hello".into()), None, user);
        let parent = items.last().map(|item| item.id);
        items.extend(expand_message_in(
            &assistant(vec![
                MsgBlock::thinking("plan".into(), None),
                MsgBlock::Text {
                    text: "answer".into(),
                },
                MsgBlock::thinking("again".into(), None),
            ]),
            parent,
            reply,
        ));
        let history = History::restored(items).unwrap();

        let ids: Vec<Value> =
            replay_history(&ToolRegistry::default(), &history, Path::new(CWD), None)
                .into_iter()
                .map(|update| serde_json::to_value(update).unwrap()["messageId"].clone())
                .collect();

        assert_eq!(
            ids,
            [
                json!(user.to_string()),
                json!(format!("{reply}{THOUGHT_ID_INFIX}0")),
                json!(reply.to_string()),
                json!(format!("{reply}{THOUGHT_ID_INFIX}1")),
            ]
        );
    }
}
