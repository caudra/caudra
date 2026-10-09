//! `memory`: the project's notes, kept across sessions.
//!
//! Every write and delete is an entry in the project's memory journal, and the
//! system prompt carries a view of the summary tree over that journal. `view`,
//! `zoom` and `search` read the tree and the journal; `read`, `write` and
//! `delete` act on one note by its name. Each call reconciles first, so a note
//! edited outside the tool is an entry before the call looks.

mod notes;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use caudra_providers::estimate_tokens;
use caudra_storage::StateDir;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::memory_journal::{
    EntryKind, EntryMeta, EntryOrigin, MemoryJournalError, body_hash,
};
use caudra_workspace::RecordScope;
use serde_json::Value;
use tracing::warn;

use crate::memory::search::terms;
use crate::memory::snapshot::EntryStatus;
use crate::memory::store::{MemoryError, MemoryStore, Source, leaf_kind, local_dir, note_name};
use crate::memory::tree::{self, Part};
use crate::permissions::{
    PermissionResource, PermissionResourceAccess, PermissionResourceKind, PermissionRisk,
    hex_encode,
};
use crate::tools::registry::{
    BoxFuture, ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionIntent,
    PermissionScopes, Tool, ToolEffect, ToolError, ToolExecResult, ToolFailure, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolContext};
use crate::types::{MemoryHit, MemoryLine, MemoryNote, MemoryOrigin, MemoryOutput, ToolOutput};

pub const DESCRIPTION: &str = "Persistent, project-scoped memory across sessions. The main session's system prompt carries it as a view of one-line summaries, oldest first, and `view` shows it as it stands now.

- Save non-obvious knowledge: gotchas and their fixes, decisions and why, where things live, commands that work.
- When a fact changes, rewrite the existing note under its name instead of adding a near-duplicate. That also brings it back to the recent end of the view.
- Delete only notes that are wrong. Nothing needs pruning: older notes fold into summaries.
- When a view line only mentions what you need, `zoom` into it or `search` before you rely on it.
- Concision comes from dropping facts, never from dropping spaces or running words together. A note that cannot be read costs more than the tokens it saved.";

pub const TOOL_USAGE: &str = "- Proactively save non-obvious project knowledge to **memory**, and rewrite a note under its name when its facts change.";

/// Tools allowed to touch the notes directory without prompting. Notes sit
/// outside the project, where an effectful tool would otherwise always ask on
/// every call.
const POLICY_TOOLS: &[&str] = &[
    crate::tools::MEMORY_TOOL_NAME,
    crate::tools::FILE_WRITE_TOOL_NAME,
    crate::tools::FILE_EDIT_TOOL_NAME,
    crate::tools::FILE_APPLY_PATCH_TOOL_NAME,
];

/// Key the rules are stored under, so a reload replaces them rather than
/// stacking duplicates.
pub const RULE_OWNER: &str = "native:memory";

/// The opaque resource a remote session's notes are named by, since a remote
/// note has no host path a filesystem rule could cover.
pub const LOCAL_MEMORY_RESOURCE: &str = "local_memory";

pub fn permission_rules(cwd: &Path) -> Vec<caudra_config::PermissionRule> {
    local_dir(cwd)
        .into_iter()
        .flat_map(|dir| {
            let scope = format!("{}/**", dir.display());
            POLICY_TOOLS
                .iter()
                .map(move |tool| caudra_config::PermissionRule {
                    tool: caudra_config::ToolKey::Native((*tool).into()),
                    scope: Some(scope.clone()),
                    effect: caudra_config::Effect::Allow,
                })
        })
        .collect()
}

const COMMANDS: &[&str] = &["view", "zoom", "search", "read", "write", "delete"];
const CWD_UNRESOLVED: &str = "cannot resolve the working directory";
const PATH_REQUIRED_FOR: &str = "'path' is required for";
const ZOOM_NEEDS_LINE: &str = "'id' and 'n' are required for zoom";
const SEARCH_NEEDS_QUERY: &str = "'query' is required for search";
const SEARCH_NEEDS_WORDS: &str = "'query' has no words to search for";
const WRITE_NEEDS_CONTENT: &str = "'content' is required for write";
const NO_HITS: &str = "No current note holds these words.";
const HIDDEN_ENTRIES: &str =
    "oldest entries are left out until their summaries are written; search finds them.";
const UNCHANGED: &str = "already says this";
const KEPT_VERSIONS: &str = "zoom keeps earlier versions";
const ENTRY_CURRENT: &str = "current";
const REWRITTEN_BY: &str = "rewritten by entry";
const DELETED_BY: &str = "deleted by entry";
const ENTRY_FORGOTTEN: &str = "its text was purged";

static COMMAND_PARAM: ParamSchema = ParamSchema::Enum {
    variants: COMMANDS,
    description: "- `view`: the memory view as it stands now.
- `zoom id n`: line id+n opened into the two lines it was made from; n=1 gives the entry whole.
- `search query`: the current notes holding the query's words, best first.
- `read path`: one note as it stands.
- `write path content`: create a note, or rewrite it whole.
- `delete path`",
};
static PATH_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Relative path, e.g. 'architecture.md'.",
};
static CONTENT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "The note's whole text.",
};
static QUERY_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Words to find in the notes' names and text.",
};
static ID_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "The id of line id+n.",
};
static N_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Integer,
    description: "The n of line id+n: how many entries it covers.",
};
static PROPERTIES: &[Property] = &[
    ("command", &COMMAND_PARAM, true, &[]),
    ("path", &PATH_PARAM, false, &[]),
    ("content", &CONTENT_PARAM, false, &[]),
    ("query", &QUERY_PARAM, false, &[]),
    ("id", &ID_PARAM, false, &[]),
    ("n", &N_PARAM, false, &[]),
];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: false,
};
static PERMISSION_CONTRACT: LazyLock<String> =
    LazyLock::new(|| super::permission_contract(&MemoryTool, ToolEffect::Mutating, DESCRIPTION));
/// `/context` is measured on every request and would otherwise re-read every
/// note each time; the cache keys on size and mtime, so a note just written
/// is never stale.
static TOKEN_CACHE: LazyLock<Mutex<notes::TokenCache>> = LazyLock::new(Mutex::default);

pub fn permission_contract() -> &'static str {
    &PERMISSION_CONTRACT
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Command {
    View,
    Zoom,
    Search,
    Read,
    Write,
    Delete,
}

impl Command {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "view" => Some(Self::View),
            "zoom" => Some(Self::Zoom),
            "search" => Some(Self::Search),
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Zoom => "zoom",
            Self::Search => "search",
            Self::Read => "read",
            Self::Write => "write",
            Self::Delete => "delete",
        }
    }

    fn access(self) -> PermissionResourceAccess {
        match self {
            Self::View | Self::Zoom | Self::Search | Self::Read => PermissionResourceAccess::Read,
            Self::Write | Self::Delete => PermissionResourceAccess::Write,
        }
    }

    fn names_note(self) -> bool {
        matches!(self, Self::Read | Self::Write | Self::Delete)
    }
}

pub struct MemoryTool;

impl Tool for MemoryTool {
    fn name(&self) -> &str {
        crate::tools::MEMORY_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn has_read_only_calls(&self) -> bool {
        true
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        Ok(Box::new(MemoryCall::parse(input, env::current_dir().ok())?))
    }
}

fn string_field(input: &Value, name: &str) -> Option<String> {
    input
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNoteInventory {
    pub name: String,
    pub on_load_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryInventory {
    pub directory: PathBuf,
    pub notes: Vec<MemoryNoteInventory>,
    pub unreadable_files: usize,
}

pub fn inventory(cwd: &Path) -> Option<MemoryInventory> {
    let directory = local_dir(cwd).ok()?;
    Some(inventory_dir(&directory, &TOKEN_CACHE))
}

fn inventory_dir(dir: &Path, cache: &Mutex<notes::TokenCache>) -> MemoryInventory {
    let (notes, warnings) = notes::scan(
        dir,
        &mut cache.lock().unwrap_or_else(PoisonError::into_inner),
    );
    MemoryInventory {
        directory: dir.to_path_buf(),
        notes: notes
            .into_iter()
            .map(|note| MemoryNoteInventory {
                name: note.name,
                on_load_tokens: note.tokens,
            })
            .collect(),
        unreadable_files: warnings.len(),
    }
}

struct MemoryCall {
    command: Command,
    /// Resolved once: permission scoping, mutation targets, and the run itself
    /// must agree on whose notes these are, and separate `current_dir` calls
    /// need not.
    cwd: Option<PathBuf>,
    path: Option<String>,
    content: Option<String>,
    query: Option<String>,
    id: Option<u64>,
    n: Option<u64>,
}

/// A call holding what its command needs. Which of the arguments that is
/// depends on the command, which the schema cannot say.
enum Request<'a> {
    View,
    Zoom { id: u64, n: u64 },
    Search(&'a str),
    Read(&'a str),
    Write { path: &'a str, content: &'a str },
    Delete(&'a str),
}

/// What a call answered with. A reading command returns what a card draws; a
/// change returns the receipt for having made it, and the note's file when it
/// is one on this host.
enum Answer {
    Found(MemoryOutput),
    Changed {
        receipt: String,
        file: Option<PathBuf>,
    },
}

impl MemoryCall {
    fn parse(input: &Value, cwd: Option<PathBuf>) -> Result<Self, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let raw = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ParseError::custom("command is required"))?;
        let command = Command::parse(raw).ok_or_else(|| {
            ParseError::custom(format!(
                "unknown command '{raw}'. Valid commands: {}",
                COMMANDS.join(", ")
            ))
        })?;
        Ok(Self {
            command,
            cwd,
            path: string_field(&input, "path"),
            content: string_field(&input, "content"),
            query: string_field(&input, "query"),
            id: input.get("id").and_then(Value::as_u64),
            n: input.get("n").and_then(Value::as_u64),
        })
    }

    fn request(&self) -> Result<Request<'_>, ToolError> {
        let path = || {
            self.path
                .as_deref()
                .ok_or_else(|| invalid(format!("{PATH_REQUIRED_FOR} {}", self.command.as_str())))
        };
        Ok(match self.command {
            Command::View => Request::View,
            Command::Zoom => {
                let (id, n) = self
                    .id
                    .zip(self.n)
                    .ok_or_else(|| invalid(ZOOM_NEEDS_LINE))?;
                Request::Zoom { id, n }
            }
            Command::Search => {
                let query = self
                    .query
                    .as_deref()
                    .ok_or_else(|| invalid(SEARCH_NEEDS_QUERY))?;
                if terms(query).is_empty() {
                    return Err(invalid(SEARCH_NEEDS_WORDS));
                }
                Request::Search(query)
            }
            Command::Read => Request::Read(path()?),
            Command::Write => Request::Write {
                path: path()?,
                content: self
                    .content
                    .as_deref()
                    .ok_or_else(|| invalid(WRITE_NEEDS_CONTENT))?,
            },
            Command::Delete => Request::Delete(path()?),
        })
    }

    /// The local project's notes, or a remote session's, which the document
    /// store keeps on this machine.
    fn store(&self, ctx: &ToolContext) -> Result<Arc<MemoryStore>, ToolError> {
        Ok(match ctx.workspace_session {
            Some(_) => MemoryStore::for_documents(scoped_store(ctx)?)?,
            None => {
                let cwd = self.cwd.as_deref().ok_or(CWD_UNRESOLVED)?;
                MemoryStore::for_cwd(&StateDir::resolve().map_err(MemoryError::from)?, cwd)?
            }
        })
    }

    /// Reconciles first, so the call sees notes edited outside the tool. A
    /// failed reconcile only means those edits arrive later.
    fn answer(
        &self,
        store: &MemoryStore,
        request: Request<'_>,
        origin: &EntryOrigin,
    ) -> Result<Answer, ToolError> {
        if let Err(error) = store.reconcile() {
            warn!(
                scope = store.scope(),
                command = self.command.as_str(),
                %error,
                "memory notes not reconciled before a call"
            );
        }
        match request {
            Request::View => view(store).map(Answer::Found),
            Request::Zoom { id, n } => zoom(store, id, n).map(Answer::Found),
            Request::Search(query) => search(store, query).map(Answer::Found),
            Request::Read(path) => read(store, path).map(Answer::Found),
            Request::Write { path, content } => write(store, path, content, origin),
            Request::Delete(path) => delete(store, path, origin),
        }
    }

    /// A reading call is drawn from its structure, and the model reads the one
    /// rendering of it that fits the context window. A change replies with its
    /// receipt; a write draws the note it stored, which the model already has.
    fn reply(&self, answer: Result<Answer, ToolError>) -> ToolExecResult {
        let (receipt, file) = match answer {
            Err(error) => return ToolExecResult::failed(error.failure, format!("error: {error}")),
            Ok(Answer::Found(output)) => {
                let text = capped_model_text(&output);
                return ToolExecResult::from(Ok(ToolOutput::Memory(output)))
                    .with_model_output(Some(text));
            }
            Ok(Answer::Changed { receipt, file }) => (receipt, file),
        };
        let note = (self.command == Command::Write)
            .then_some(self.content.as_deref())
            .flatten()
            .filter(|content| !content.trim().is_empty());
        // Notes live outside the project, where no watch sees them change, so
        // the host learns of the change from the result.
        let changed = file.map(|file| file.to_string_lossy().into_owned());
        match note {
            Some(note) => ToolExecResult::from(Ok(ToolOutput::Markdown(note.into())))
                .with_model_output(Some(receipt)),
            None => ToolExecResult::from(Ok(ToolOutput::Markdown(receipt.into()))),
        }
        .with_written_path(changed)
    }

    /// What the call acts on, as its header and a remote permission name it.
    fn subject(&self) -> Option<String> {
        match self.command {
            Command::View => None,
            Command::Zoom => self.id.zip(self.n).map(|(id, n)| format!("{id}+{n}")),
            Command::Search => self.query.clone(),
            Command::Read | Command::Write | Command::Delete => self.path.clone(),
        }
    }

    fn notes_dir(&self) -> Option<PathBuf> {
        local_dir(self.cwd.as_deref()?).ok()
    }

    /// The file of the note the call names, for the commands that name one.
    fn note_file(&self, dir: &Path) -> Option<PathBuf> {
        let path = self.path.as_deref().filter(|_| self.command.names_note())?;
        Some(dir.join(note_name(path).ok()?))
    }

    fn remote_permission_intent(&self) -> PermissionIntent {
        let scope = format!(
            "local-memory:{}:{}",
            self.command.as_str(),
            self.subject().unwrap_or_default()
        );
        PermissionIntent::new(
            PermissionScopes::single(scope.clone()),
            vec![PermissionResource {
                kind: PermissionResourceKind::Custom {
                    name: LOCAL_MEMORY_RESOURCE.to_owned(),
                },
                value: scope,
                access: Some(self.command.access()),
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            PermissionRisk::Low,
        )
    }
}

impl ToolInvocation for MemoryCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(match self.subject() {
            Some(subject) => format!("{} {subject}", self.command.as_str()),
            None => self.command.as_str().to_owned(),
        }))
    }

    fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
        Box::pin(async move {
            let dir = self.notes_dir()?;
            // A call naming a note approves that file; the rest read the whole
            // directory.
            let scope = self.note_file(&dir).unwrap_or_else(|| dir.join("**"));
            Some(PermissionScopes {
                scopes: vec![scope.to_string_lossy().into_owned()],
                force_prompt: false,
                plan_scoped: false,
            })
        })
    }

    fn preflight<'a>(
        &'a self,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Option<PermissionIntent>, ToolError>> {
        Box::pin(async move {
            if ctx.workspace_session.is_none() {
                return Ok(None);
            }
            scoped_store(ctx)?;
            Ok(Some(self.remote_permission_intent()))
        })
    }

    /// Reading the memory is a read; only the two commands that change a note
    /// carry the registered mutating effect.
    fn call_effect(&self, registered: ToolEffect) -> ToolEffect {
        match self.command {
            Command::View | Command::Zoom | Command::Search | Command::Read => ToolEffect::ReadOnly,
            Command::Write | Command::Delete => registered,
        }
    }

    fn mutation_targets(&self, ctx: &ToolContext) -> Vec<PathBuf> {
        if ctx.workspace_session.is_some()
            || self.command.access() == PermissionResourceAccess::Read
        {
            return Vec::new();
        }
        self.notes_dir()
            .and_then(|dir| self.note_file(&dir))
            .into_iter()
            .collect()
    }

    /// Notes are Caudra's own state, which no session's file revert covers.
    fn record_scope(&self, _ctx: &ToolContext, _root: &Path) -> Option<RecordScope> {
        None
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let answer = self.request().and_then(|request| {
                let store = self.store(ctx)?;
                self.answer(&store, request, &entry_origin(ctx))
            });
            self.reply(answer)
        })
    }
}

fn scoped_store(ctx: &ToolContext) -> Result<&Arc<LocalDocumentStore>, ToolError> {
    let workspace = ctx
        .workspace_session
        .as_ref()
        .ok_or("remote memory requires a remote workspace session")?;
    let store = ctx
        .local_documents
        .as_ref()
        .ok_or("local document store is unavailable")?;
    store.validate_binding(workspace.binding())?;
    Ok(store)
}

fn entry_origin(ctx: &ToolContext) -> EntryOrigin {
    ctx.session_id.as_ref().map_or(EntryOrigin::External, |id| {
        EntryOrigin::Session(id.as_str().to_owned())
    })
}

fn view(store: &MemoryStore) -> Result<MemoryOutput, ToolError> {
    let block = store.view()?;
    let mut notices = Vec::new();
    if block.hidden > 0 {
        notices.push(format!("The {} {HIDDEN_ENTRIES}", block.hidden));
    } else if block.lines.is_empty() {
        notices.push(notes::NO_MEMORIES.to_owned());
    }
    Ok(MemoryOutput::Lines {
        heading: view_heading(block.through(), block.lines.len()),
        lines: block
            .lines
            .iter()
            .map(|line| memory_line(&line.part, &line.text, line.pending()))
            .collect(),
        notices,
    })
}

/// `548 entries in 72 lines, oldest first:`.
fn view_heading(entries: u64, lines: usize) -> String {
    let entries = match entries {
        1 => "1 entry".to_owned(),
        entries => format!("{entries} entries"),
    };
    let lines = match lines {
        1 => "1 line".to_owned(),
        lines => format!("{lines} lines"),
    };
    format!("{entries} in {lines}, oldest first:")
}

/// The two lines `id+n` was made from, or for `n` = 1 the entry whole and
/// what became of it.
fn zoom(store: &MemoryStore, id: u64, n: u64) -> Result<MemoryOutput, ToolError> {
    let entries = store
        .journal()
        .len(store.scope())
        .map_err(MemoryError::from)?;
    let part = tree::zoom(id, n, entries).map_err(|error| invalid(error.to_string()))?;
    let Some(children) = part.children() else {
        return entry(store, id);
    };
    let tree = store.load()?.tree;
    Ok(MemoryOutput::Lines {
        heading: format!("{part} was made from:"),
        lines: children
            .iter()
            .map(|child| memory_line(child, &tree.line(child), !tree.is_built(child)))
            .collect(),
        notices: Vec::new(),
    })
}

fn entry(store: &MemoryStore, seq: u64) -> Result<MemoryOutput, ToolError> {
    let scope = store.scope();
    let (entry, later) = store
        .journal()
        .read(|journal| Ok((journal.entry(scope, seq)?, journal.entries(scope, seq + 1)?)))
        .map_err(MemoryError::from)?;
    let entry = entry.ok_or_else(|| {
        ToolError::new(ToolFailure::NotFound, format!("entry {seq} does not exist"))
    })?;
    let notice = status_notice(&entry.meta, status(&entry.meta, &later));
    Ok(MemoryOutput::Notes {
        directory: directory(store),
        notes: Vec::from([memory_note(store, &entry.meta.name, entry.body)?]),
        notices: Vec::from([notice]),
    })
}

/// What became of the entry, told by the later entries of the same name.
fn status(meta: &EntryMeta, later: &[EntryMeta]) -> EntryStatus {
    if meta.forgotten {
        return EntryStatus::Forgotten;
    }
    match later.iter().find(|next| next.name == meta.name) {
        None => EntryStatus::Current,
        Some(next) => match next.kind {
            EntryKind::Note => EntryStatus::Rewritten(next.seq),
            EntryKind::Delete => EntryStatus::Deleted(next.seq),
        },
    }
}

/// `Entry 12, note ci.md: rewritten by entry 40.`
fn status_notice(meta: &EntryMeta, status: EntryStatus) -> String {
    let status = match status {
        EntryStatus::Current => ENTRY_CURRENT.to_owned(),
        EntryStatus::Rewritten(seq) => format!("{REWRITTEN_BY} {seq}"),
        EntryStatus::Deleted(seq) => format!("{DELETED_BY} {seq}"),
        EntryStatus::Forgotten => ENTRY_FORGOTTEN.to_owned(),
    };
    format!(
        "Entry {}, {} {}: {status}.",
        meta.seq,
        leaf_kind(meta).as_str(),
        meta.name
    )
}

fn search(store: &MemoryStore, query: &str) -> Result<MemoryOutput, ToolError> {
    let hits: Vec<MemoryHit> = store
        .search(query)?
        .into_iter()
        .map(|hit| MemoryHit {
            seq: hit.seq,
            path: local_file(store, &hit.name).map(|file| file.to_string_lossy().into_owned()),
            name: hit.name,
            heading: hit.heading,
            line: hit.line,
        })
        .collect();
    let notices = match hits.is_empty() {
        true => Vec::from([NO_HITS.to_owned()]),
        false => Vec::new(),
    };
    Ok(MemoryOutput::Hits {
        query: query.to_owned(),
        hits,
        notices,
    })
}

fn read(store: &MemoryStore, path: &str) -> Result<MemoryOutput, ToolError> {
    let name = note_name(path)?;
    let entry = store
        .journal()
        .live(store.scope(), &name)
        .map_err(MemoryError::from)?
        .ok_or_else(|| missing(&name))?;
    Ok(MemoryOutput::Notes {
        directory: directory(store),
        notes: Vec::from([memory_note(store, &name, entry.body)?]),
        notices: Vec::new(),
    })
}

fn write(
    store: &MemoryStore,
    path: &str,
    content: &str,
    origin: &EntryOrigin,
) -> Result<Answer, ToolError> {
    if let Some(error) = notes::write_size_error(content) {
        return Err(invalid(error));
    }
    let name = note_name(path)?;
    let receipt = match store.write(&name, content, origin)? {
        Some(seq) => format!("wrote {name} (entry {seq})"),
        None => format!("{name} {UNCHANGED}"),
    };
    Ok(Answer::Changed {
        receipt,
        file: local_file(store, &name),
    })
}

fn delete(store: &MemoryStore, path: &str, origin: &EntryOrigin) -> Result<Answer, ToolError> {
    let name = note_name(path)?;
    let receipt = match store.delete(&name, origin) {
        Ok(Some(seq)) => format!("deleted {name} (entry {seq}; {KEPT_VERSIONS})"),
        Ok(None) => format!("deleted {name}"),
        Err(error) if not_found(&error) => return Err(missing(&name)),
        Err(error) => return Err(error.into()),
    };
    Ok(Answer::Changed {
        receipt,
        file: local_file(store, &name),
    })
}

fn not_found(error: &MemoryError) -> bool {
    matches!(
        error,
        MemoryError::Io(error) | MemoryError::Journal(MemoryJournalError::Io(error))
            if error.kind() == io::ErrorKind::NotFound
    )
}

/// The notes directory, which a reader can edit notes in. A remote session's
/// notes have no path to name.
fn directory(store: &MemoryStore) -> Option<String> {
    match store.source() {
        Source::Local(dir) => Some(dir.display().to_string()),
        Source::Documents(_) => None,
    }
}

fn local_file(store: &MemoryStore, name: &str) -> Option<PathBuf> {
    match store.source() {
        Source::Local(dir) => Some(dir.join(name)),
        Source::Documents(_) => None,
    }
}

/// A note as the card and the model both read it. A note on this host is
/// named by its file; a remote one by its reference and the revision of the
/// text shown, since it has no path on this host.
fn memory_note(store: &MemoryStore, name: &str, body: String) -> Result<MemoryNote, ToolError> {
    let origin = match store.source() {
        Source::Local(dir) => MemoryOrigin::File {
            path: dir.join(name).to_string_lossy().into_owned(),
        },
        Source::Documents(documents) => MemoryOrigin::Document {
            reference: documents
                .memory_reference(documents.project_key(), name)?
                .as_str()
                .to_owned(),
            revision: hex_encode(&body_hash(&body)),
        },
    };
    Ok(MemoryNote {
        name: name.to_owned(),
        tokens: estimate_tokens(&body),
        tags: Vec::new(),
        origin,
        body,
    })
}

fn memory_line(part: &Part, text: &str, pending: bool) -> MemoryLine {
    MemoryLine {
        id: part.start(),
        count: part.count(),
        text: text.replace(['\n', '\r'], " "),
        pending,
    }
}

/// How much of an answer the model is charged for. The card draws from the
/// structure and has its own row budget, so the byte cap is only ever about
/// what a result costs the context window.
fn capped_model_text(output: &MemoryOutput) -> String {
    let hint = match output {
        MemoryOutput::Notes { .. } => notes::CAP_HINT_REWRITE,
        MemoryOutput::Lines { .. } | MemoryOutput::Hits { .. } | MemoryOutput::Index { .. } => {
            notes::CAP_HINT_ZOOM
        }
    };
    notes::cap(output.as_display_text(), hint)
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolFailure::InvalidInput, message)
}

fn missing(name: &str) -> ToolError {
    ToolError::new(ToolFailure::NotFound, format!("'{name}' does not exist"))
}

impl From<MemoryError> for ToolError {
    fn from(error: MemoryError) -> Self {
        if let MemoryError::Documents(error) = error {
            return error.into();
        }
        let failure = match &error {
            MemoryError::InvalidName { .. }
            | MemoryError::TooLarge { .. }
            | MemoryError::Journal(MemoryJournalError::TooLarge { .. }) => {
                ToolFailure::InvalidInput
            }
            MemoryError::Io(error) | MemoryError::Journal(MemoryJournalError::Io(error)) => {
                ToolFailure::from(error)
            }
            MemoryError::State(_) | MemoryError::Journal(_) | MemoryError::Documents(_) => {
                ToolFailure::Other
            }
        };
        Self::new(failure, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use caudra_config::{
        DefaultEffect, Effect, FeatureFlags, PermissionRule, PermissionsConfig, ToolKey,
    };
    use caudra_storage::id::SessionRef;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::AgentMode;
    use crate::agent::tool_dispatch::{self, Emit};
    use crate::memory::store::{NAME_NOT_MARKDOWN, NAME_TRAVERSAL};
    use crate::memory::tree::{NODE, PENDING, VIEW, ZoomError};
    use crate::permissions::{
        PERMISSION_DENIED_PREFIX, PermissionExecutorKind, PermissionManager, PermissionSubject,
    };
    use crate::tools::native::OWNER;
    use crate::tools::native::tests::{tempdir, workspace_for_principal};
    use crate::tools::test_support::stub_ctx;
    use crate::tools::{MEMORY_TOOL_NAME, PLAN_WRITE_RESTRICTED};
    use crate::types::MEMORY_DIRECTORY_LABEL;

    const REGISTERED_EFFECT: ToolEffect = ToolEffect::Mutating;
    const SCOPE: &str = "projects/test";
    const MEMORIES: &str = "memories";
    const SESSION: &str = "session-a";
    const NOTE: &str = "a.md";
    const OTHER: &str = "b.md";
    const BODY: &str = "# Alpha\nfirst body";
    const EDITED: &str = "# Alpha\nedited body";
    const NOTE_BODY: &str = "# Session picker\n\nThe picker merges **two** sources.";
    const NOTE_PATH: &str = "note.md";
    const NOTE_CONTENT: &str = "retained memory";
    const WRONG_OWNER: &str = "local document does not belong to this project or session";
    const FOUND_MSG: &str = "a reading command answers with what a card draws";
    const BODY_MSG: &str = "the reader sees the note, the model sees the receipt";
    const CHANGED_NOTE_MSG: &str = "a local change names its note, and nothing else names one";
    const INCOHERENT_MSG: &str = "a call missing what its command needs must not run";

    struct Fixture {
        _state: TempDir,
        dir: PathBuf,
        store: MemoryStore,
    }

    /// A store on a temporary state directory, which is the only way to run a
    /// command without touching the developer's real notes.
    fn fixture() -> Fixture {
        let state = tempdir();
        let dir = state.path().join(MEMORIES);
        let store = MemoryStore::open(
            &StateDir::from_path(state.path().to_path_buf()),
            SCOPE.to_owned(),
            Source::Local(dir.clone()),
        )
        .unwrap();
        Fixture {
            _state: state,
            dir,
            store,
        }
    }

    impl Fixture {
        fn answer(&self, input: Value) -> Result<Answer, ToolError> {
            let call = call(input);
            call.request()
                .and_then(|request| call.answer(&self.store, request, &session()))
        }

        /// What the model reads: a reading command's one rendering, or a
        /// change's receipt.
        fn text(&self, input: Value) -> Result<String, ToolError> {
            self.answer(input).map(|answer| match answer {
                Answer::Found(output) => capped_model_text(&output),
                Answer::Changed { receipt, .. } => receipt,
            })
        }

        fn found(&self, input: Value) -> MemoryOutput {
            match self.answer(input) {
                Ok(Answer::Found(output)) => output,
                Ok(Answer::Changed { receipt, .. }) => panic!("{FOUND_MSG}, got {receipt}"),
                Err(error) => panic!("{FOUND_MSG}, got {error}"),
            }
        }

        fn execute(&self, input: Value) -> ToolExecResult {
            let call = call(input);
            call.reply(
                call.request()
                    .and_then(|request| call.answer(&self.store, request, &session())),
            )
        }

        fn write(&self, name: &str, content: &str) {
            self.text(write_input(name, content)).unwrap();
        }

        fn put(&self, name: &str, content: &str) {
            fs::create_dir_all(&self.dir).unwrap();
            fs::write(self.dir.join(name), content).unwrap();
        }
    }

    fn session() -> EntryOrigin {
        EntryOrigin::Session(SESSION.to_owned())
    }

    fn call(input: Value) -> MemoryCall {
        MemoryCall::parse(&input, None).expect("valid input")
    }

    fn call_at(input: Value, cwd: &Path) -> MemoryCall {
        MemoryCall::parse(&input, Some(cwd.to_path_buf())).expect("valid input")
    }

    fn write_input(path: &str, content: &str) -> Value {
        json!({ "command": "write", "path": path, "content": content })
    }

    fn zoom_input(id: u64, n: u64) -> Value {
        json!({ "command": "zoom", "id": id, "n": n })
    }

    fn markdown(result: ToolExecResult) -> String {
        match result.output.expect("a call that succeeded") {
            ToolOutput::Markdown(text) => text.text,
            other => panic!("{BODY_MSG}, got {other:?}"),
        }
    }

    #[test]
    fn a_write_and_a_delete_name_the_entries_they_made() {
        let notes = fixture();

        assert_eq!(
            notes.text(write_input(NOTE, BODY)).unwrap(),
            format!("wrote {NOTE} (entry 0)")
        );
        assert_eq!(fs::read_to_string(notes.dir.join(NOTE)).unwrap(), BODY);
        assert_eq!(
            notes
                .text(json!({ "command": "delete", "path": NOTE }))
                .unwrap(),
            format!("deleted {NOTE} (entry 1; {KEPT_VERSIONS})")
        );
        assert!(!notes.dir.join(NOTE).exists());
    }

    #[test]
    fn rewriting_a_note_with_its_own_text_adds_no_entry() {
        let notes = fixture();
        notes.write(NOTE, BODY);

        assert_eq!(
            notes.text(write_input(NOTE, BODY)).unwrap(),
            format!("{NOTE} {UNCHANGED}")
        );
        assert_eq!(notes.store.journal().len(SCOPE).unwrap(), 1);
    }

    #[test]
    fn a_change_is_recorded_as_the_calling_sessions() {
        let mut ctx = stub_ctx(&AgentMode::Build);
        assert_eq!(entry_origin(&ctx), EntryOrigin::External);

        let session = SessionRef::generate();
        let expected = EntryOrigin::Session(session.as_str().to_owned());
        ctx.session_id = Some(session);

        assert_eq!(entry_origin(&ctx), expected);
    }

    /// A note edited outside the tool is an entry before the call reads, and
    /// reads back as the journal keeps it: without the tags earlier releases
    /// kept in frontmatter.
    #[test]
    fn a_read_returns_a_note_edited_elsewhere_without_its_frontmatter() {
        let notes = fixture();
        notes.put(NOTE, &format!("---\ntags: [ui]\n---\n{NOTE_BODY}"));

        let MemoryOutput::Notes {
            notes: read,
            notices,
            directory,
        } = notes.found(json!({ "command": "read", "path": NOTE }))
        else {
            panic!("{FOUND_MSG}");
        };

        assert!(notices.is_empty());
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].body, NOTE_BODY);
        assert!(read[0].tags.is_empty());
        assert_eq!(
            read[0].origin.path(),
            Some(notes.dir.join(NOTE).to_string_lossy().as_ref())
        );
        assert_eq!(directory, Some(notes.dir.display().to_string()));
    }

    #[test_case(json!({ "command": "read", "path": "gone.md" }) ; "reading")]
    #[test_case(json!({ "command": "delete", "path": "gone.md" }) ; "deleting")]
    fn a_missing_note_is_not_found(input: Value) {
        let notes = fixture();
        notes.write(NOTE, BODY);

        let error = notes.text(input).unwrap_err();

        assert_eq!(error.failure, ToolFailure::NotFound);
        assert_eq!(error.message, missing("gone.md").message);
    }

    #[test]
    fn a_traversing_path_is_refused_before_anything_is_written() {
        let notes = fixture();

        let error = notes.text(write_input("../escape.md", BODY)).unwrap_err();

        assert_eq!(error.failure, ToolFailure::InvalidInput);
        assert_eq!(error.message, NAME_TRAVERSAL);
        assert!(!notes.dir.with_file_name("escape.md").exists());
    }

    #[test]
    fn an_oversized_write_never_reaches_disk() {
        let notes = fixture();

        let error = notes
            .text(write_input(NOTE, &"x".repeat(notes::MAX_FILE_BYTES + 1)))
            .unwrap_err();

        assert_eq!(error.failure, ToolFailure::InvalidInput);
        assert!(!notes.dir.join(NOTE).exists());
    }

    #[test_case(json!({ "command": "read" }), format!("{PATH_REQUIRED_FOR} read") ; "read_needs_a_path")]
    #[test_case(json!({ "command": "write", "content": "x" }), format!("{PATH_REQUIRED_FOR} write") ; "write_needs_a_path")]
    #[test_case(json!({ "command": "write", "path": NOTE }), WRITE_NEEDS_CONTENT.to_owned() ; "write_needs_content")]
    #[test_case(json!({ "command": "delete" }), format!("{PATH_REQUIRED_FOR} delete") ; "delete_needs_a_path")]
    #[test_case(json!({ "command": "zoom", "id": 0 }), ZOOM_NEEDS_LINE.to_owned() ; "zoom_needs_n")]
    #[test_case(json!({ "command": "zoom", "id": -1, "n": 1 }), ZOOM_NEEDS_LINE.to_owned() ; "zoom_needs_a_whole_id")]
    #[test_case(json!({ "command": "search" }), SEARCH_NEEDS_QUERY.to_owned() ; "search_needs_a_query")]
    #[test_case(json!({ "command": "search", "query": "-- !" }), SEARCH_NEEDS_WORDS.to_owned() ; "search_needs_words")]
    fn an_incoherent_call_is_rejected(input: Value, expected: String) {
        let Err(error) = call(input).request() else {
            panic!("{INCOHERENT_MSG}");
        };
        assert_eq!(error.failure, ToolFailure::InvalidInput);
        assert_eq!(error.message, expected);
    }

    #[test_case("list" ; "the_retired_list")]
    #[test_case("tags" ; "the_retired_tags")]
    fn an_unknown_command_is_refused_with_the_valid_ones(command: &str) {
        let Err(error) = MemoryTool.parse(&json!({ "command": command })) else {
            panic!("an unknown command must not parse");
        };
        let error = error.to_string();
        assert!(error.contains(command), "{error}");
        for valid in COMMANDS {
            assert!(error.contains(valid), "{error}");
        }
    }

    #[test_case(json!({ "command": "view" }), "view" ; "view")]
    #[test_case(json!({ "command": "zoom", "id": 368, "n": 8 }), "zoom 368+8" ; "zoom")]
    #[test_case(json!({ "command": "search", "query": "flaky tests" }), "search flaky tests" ; "search")]
    #[test_case(json!({ "command": "read", "path": NOTE }), "read a.md" ; "read")]
    fn the_header_describes_the_call(input: Value, expected: &str) {
        let HeaderResult::Plain(header) = smol::block_on(call(input).start_header()) else {
            panic!("memory headers are plain");
        };
        assert_eq!(header, expected);
    }

    #[test]
    fn a_view_lists_one_addressed_line_per_entry_under_its_heading() {
        let notes = fixture();
        notes.write(NOTE, BODY);
        notes.write(OTHER, EDITED);

        assert_eq!(
            notes.text(json!({ "command": "view" })).unwrap(),
            format!(
                "{}\n0+1|note {NOTE} {}\n1+1|note {OTHER} {}",
                view_heading(2, 2),
                BODY.replace('\n', " "),
                EDITED.replace('\n', " ")
            )
        );
    }

    #[test]
    fn an_empty_view_says_so() {
        let output = fixture().found(json!({ "command": "view" }));

        assert!(output.is_empty());
        assert_eq!(output.notices(), [notes::NO_MEMORIES.to_owned()]);
    }

    /// Entries kept word for word and too long to merge without a model leave
    /// the view over budget until they are summarized. What is left fills the
    /// budget, and the model still reads all of it.
    #[test]
    fn a_view_over_budget_says_how_many_of_the_oldest_entries_it_leaves_out() {
        let notes = fixture();
        let body = "x".repeat(NODE - 32);
        for index in 0..VIEW / NODE + 4 {
            notes.write(&format!("n{index}.md"), &body);
        }

        let output = notes.found(json!({ "command": "view" }));

        let [notice] = output.notices() else {
            panic!("one notice: {:?}", output.notices());
        };
        assert!(notice.ends_with(HIDDEN_ENTRIES), "{notice}");
        assert_eq!(capped_model_text(&output), output.as_display_text());
    }

    #[test_case(1, 1, "1 entry in 1 line, oldest first:" ; "one_of_each")]
    #[test_case(548, 72, "548 entries in 72 lines, oldest first:" ; "many")]
    fn a_view_heading_counts_entries_and_lines(entries: u64, lines: usize, expected: &str) {
        assert_eq!(view_heading(entries, lines), expected);
    }

    #[test]
    fn zoom_opens_a_line_into_the_two_it_was_made_from() {
        let notes = fixture();
        notes.write(NOTE, BODY);
        notes.write(OTHER, &"x".repeat(NODE));

        let MemoryOutput::Lines { heading, lines, .. } = notes.found(zoom_input(0, 2)) else {
            panic!("{FOUND_MSG}");
        };

        assert_eq!(heading, "0+2 was made from:");
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.id, line.count, line.pending))
                .collect::<Vec<_>>(),
            [(0, 1, false), (1, 1, true)]
        );
        assert_eq!(
            lines[0].text,
            format!("note {NOTE} {}", BODY.replace('\n', " "))
        );
        assert!(lines[1].text.starts_with(PENDING), "{}", lines[1].text);
    }

    /// Entries 0 to 3: `a.md` written then rewritten, `b.md` written then
    /// deleted.
    fn history() -> Fixture {
        let notes = fixture();
        notes.write(NOTE, BODY);
        notes.write(NOTE, EDITED);
        notes.write(OTHER, BODY);
        notes
            .text(json!({ "command": "delete", "path": OTHER }))
            .unwrap();
        notes
    }

    #[test_case(0, BODY, format!("note {NOTE}: {REWRITTEN_BY} 1") ; "a_rewritten_note")]
    #[test_case(1, EDITED, format!("note {NOTE}: {ENTRY_CURRENT}") ; "the_current_note")]
    #[test_case(2, BODY, format!("note {OTHER}: {DELETED_BY} 3") ; "a_deleted_note")]
    #[test_case(3, "", format!("delete {OTHER}: {ENTRY_CURRENT}") ; "the_deletion_itself")]
    fn zooming_into_one_entry_reads_it_whole_with_what_became_of_it(
        seq: u64,
        body: &str,
        status: String,
    ) {
        let MemoryOutput::Notes {
            notes: read,
            notices,
            ..
        } = history().found(zoom_input(seq, 1))
        else {
            panic!("{FOUND_MSG}");
        };

        assert_eq!(notices, [format!("Entry {seq}, {status}.")]);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].body, body);
    }

    #[test]
    fn a_forgotten_entry_says_its_text_is_gone() {
        let notes = fixture();
        notes.write(NOTE, BODY);
        notes.store.forget(NOTE).unwrap();

        let MemoryOutput::Notes {
            notes: read,
            notices,
            ..
        } = notes.found(zoom_input(0, 1))
        else {
            panic!("{FOUND_MSG}");
        };

        assert_eq!(
            notices,
            [format!("Entry 0, forgotten {NOTE}: {ENTRY_FORGOTTEN}.")]
        );
        assert!(!read[0].body.contains(BODY), "{}", read[0].body);
    }

    #[test_case(0, 3, ZoomError::NotPowerOfTwo(3) ; "n_not_a_power_of_two")]
    #[test_case(1, 2, ZoomError::Misaligned { id: 1, n: 2 } ; "id_not_a_multiple_of_n")]
    #[test_case(0, 4, ZoomError::PastEnd { id: 0, n: 4, entries: 2 } ; "past_the_last_entry")]
    fn a_zoom_off_the_tree_is_refused(id: u64, n: u64, expected: ZoomError) {
        let notes = fixture();
        notes.write(NOTE, BODY);
        notes.write(OTHER, BODY);

        let error = notes.text(zoom_input(id, n)).unwrap_err();

        assert_eq!(error.failure, ToolFailure::InvalidInput);
        assert_eq!(error.message, expected.to_string());
    }

    /// A word in a note's name outweighs one in its text, and of two notes
    /// that match alike the newer comes first.
    #[test]
    fn search_ranks_name_matches_first_then_the_newest() {
        let notes = fixture();
        notes.write("flaky-tests.md", "# Flaky tests\nRetry the suite once.");
        notes.write("ci.md", "# CI\nThe flaky suite runs nightly.");
        notes.write("release.md", "# Release\nA flaky upload retries.");
        notes.write("unrelated.md", "# Other\nNothing to see.");

        let MemoryOutput::Hits { hits, notices, .. } =
            notes.found(json!({ "command": "search", "query": "Flaky" }))
        else {
            panic!("{FOUND_MSG}");
        };

        assert!(notices.is_empty());
        assert_eq!(
            hits.iter()
                .map(|hit| (hit.seq, hit.name.as_str()))
                .collect::<Vec<_>>(),
            [(0, "flaky-tests.md"), (2, "release.md"), (1, "ci.md")]
        );
        assert_eq!(hits[1].heading, "Release");
        assert_eq!(hits[1].line.as_deref(), Some("A flaky upload retries."));
        let release = notes.dir.join("release.md");
        assert_eq!(
            hits[1].path.as_deref(),
            Some(release.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn a_search_that_finds_nothing_says_so() {
        let notes = fixture();
        notes.write(NOTE, BODY);

        let output = notes.found(json!({ "command": "search", "query": "absent" }));

        assert!(output.is_empty());
        assert_eq!(output.as_display_text(), NO_HITS);
    }

    /// A reading call has one answer, so both sides read it: the reader gets
    /// the structure the card draws, and the model the one rendering of it.
    #[test_case(json!({ "command": "view" }) ; "a_view")]
    #[test_case(zoom_input(0, 1) ; "a_zoom")]
    #[test_case(json!({ "command": "search", "query": "picker" }) ; "a_search")]
    #[test_case(json!({ "command": "read", "path": NOTE }) ; "a_read")]
    fn a_reading_call_shows_the_model_what_the_reader_sees(input: Value) {
        let notes = fixture();
        notes.write(NOTE, NOTE_BODY);

        let out = notes.execute(input);

        let Ok(ToolOutput::Memory(output)) = &out.output else {
            panic!("{FOUND_MSG}");
        };
        assert_eq!(
            out.model_output.as_deref(),
            Some(output.as_display_text().as_str())
        );
        assert!(output.as_display_text().contains("picker"));
    }

    #[test]
    fn a_read_reports_the_directory_so_notes_can_be_edited() {
        let notes = fixture();
        notes.write(NOTE, BODY);

        let out = notes
            .text(json!({ "command": "read", "path": NOTE }))
            .unwrap();

        assert!(
            out.starts_with(&format!("{MEMORY_DIRECTORY_LABEL}{}", notes.dir.display())),
            "{out}"
        );
    }

    /// A write's reply is a receipt, so rendering it is rendering nothing. The
    /// note is what the reader came for, and the model wrote it and does not
    /// need it back.
    #[test]
    fn a_write_draws_the_note_and_replies_with_its_receipt() {
        let out = fixture().execute(write_input(NOTE, NOTE_BODY));

        assert_eq!(out.model_output, Some(format!("wrote {NOTE} (entry 0)")));
        assert_eq!(markdown(out), NOTE_BODY, "{BODY_MSG}");
    }

    /// Nothing to render is not a reason to render nothing: an empty note
    /// would leave the row with no body at all, so the receipt stands in.
    #[test]
    fn a_blank_note_falls_back_to_its_receipt() {
        let out = fixture().execute(write_input(NOTE, "  \n "));

        assert_eq!(out.model_output, None, "{BODY_MSG}");
        assert_eq!(markdown(out), format!("wrote {NOTE} (entry 0)"));
    }

    /// The notes live outside the project, so the host only learns a tab on
    /// one is stale from the call that changed it.
    #[test_case(write_input(NOTE, EDITED), true ; "a_write")]
    #[test_case(json!({ "command": "delete", "path": NOTE }), true ; "a_delete")]
    #[test_case(json!({ "command": "read", "path": NOTE }), false ; "a_read")]
    fn a_local_change_names_the_note_it_changed(input: Value, changes: bool) {
        let notes = fixture();
        notes.write(NOTE, BODY);

        let out = notes.execute(input);

        let expected = changes.then(|| notes.dir.join(NOTE).to_string_lossy().into_owned());
        assert_eq!(out.written_path, expected, "{CHANGED_NOTE_MSG}");
    }

    /// The call is refused before any store is opened, so a bad call never
    /// touches the notes.
    #[test]
    fn a_failing_call_is_reported_as_a_tool_error() {
        let parsed = MemoryTool
            .parse(&json!({ "command": "read" }))
            .expect("valid input");

        let out = smol::block_on(parsed.execute(&stub_ctx(&AgentMode::Build)));

        assert!(out.is_error);
        assert_eq!(out.failure, Some(ToolFailure::InvalidInput));
        assert!(out.output.unwrap_err().starts_with("error: "));
    }

    /// Write and delete declare their target so the permission layer can see
    /// the file before it changes.
    #[test_case(write_input(NOTE, BODY), true ; "a_write")]
    #[test_case(json!({ "command": "delete", "path": NOTE }), true ; "a_delete")]
    #[test_case(json!({ "command": "read", "path": NOTE }), false ; "a_read")]
    #[test_case(json!({ "command": "view" }), false ; "a_view")]
    fn a_mutating_call_declares_the_file_it_touches(input: Value, mutates: bool) {
        let cwd = tempdir();
        let dir = local_dir(cwd.path()).unwrap();

        let targets = call_at(input, cwd.path()).mutation_targets(&stub_ctx(&AgentMode::Build));

        let expected: Vec<PathBuf> = mutates.then(|| dir.join(NOTE)).into_iter().collect();
        assert_eq!(targets, expected);
    }

    #[test]
    fn a_note_is_never_recorded_even_inside_the_session_directory() {
        let cwd = tempdir();
        let write = call_at(write_input(NOTE, BODY), cwd.path());
        assert_eq!(
            write.record_scope(&stub_ctx(&AgentMode::Build), cwd.path()),
            None
        );
    }

    /// The registration is mutating so a change is gated; reading has to
    /// report itself as the read it is or plan mode refuses it.
    #[test_case("view", ToolEffect::ReadOnly ; "view_is_a_read")]
    #[test_case("zoom", ToolEffect::ReadOnly ; "zoom_is_a_read")]
    #[test_case("search", ToolEffect::ReadOnly ; "search_is_a_read")]
    #[test_case("read", ToolEffect::ReadOnly ; "read_is_a_read")]
    #[test_case("write", REGISTERED_EFFECT ; "write_keeps_the_registered_effect")]
    #[test_case("delete", REGISTERED_EFFECT ; "delete_keeps_the_registered_effect")]
    fn the_call_effect_follows_the_command(command: &str, expected: ToolEffect) {
        let parsed = call(json!({ "command": command, "path": NOTE, "content": "x" }));
        assert_eq!(parsed.call_effect(REGISTERED_EFFECT), expected);
    }

    #[test_case(json!({ "command": "view" }) ; "a_view")]
    #[test_case(json!({ "command": "search", "query": "x", "path": NOTE }) ; "a_search")]
    #[test_case(zoom_input(0, 1) ; "a_zoom")]
    fn a_call_naming_no_note_asks_for_the_whole_directory(input: Value) {
        let cwd = tempdir();
        let dir = local_dir(cwd.path()).unwrap();

        let scopes = smol::block_on(call_at(input, cwd.path()).permission_scopes()).unwrap();

        assert_eq!(
            scopes.scopes,
            vec![dir.join("**").to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn a_call_naming_a_note_asks_only_for_that_file() {
        let cwd = tempdir();
        let dir = local_dir(cwd.path()).unwrap();
        let read = call_at(json!({ "command": "read", "path": NOTE }), cwd.path());

        let scopes = smol::block_on(read.permission_scopes()).unwrap();

        assert_eq!(
            scopes.scopes,
            vec![dir.join(NOTE).to_string_lossy().into_owned()]
        );
    }

    /// Every note-touching tool needs a rule for every directory notes can
    /// live in, or the model gets a prompt for its own scratchpad.
    #[test]
    fn permission_rules_cover_each_policy_tool() {
        let temp = tempdir();
        let rules = permission_rules(temp.path());
        assert!(!rules.is_empty(), "the state directory always resolves");
        for tool in POLICY_TOOLS {
            assert!(
                rules.iter().any(|rule| {
                    matches!(&rule.tool, caudra_config::ToolKey::Native(name) if &**name == *tool)
                }),
                "no rule for {tool}"
            );
        }
        assert!(
            rules
                .iter()
                .all(|rule| rule.effect == caudra_config::Effect::Allow),
            "memory rules only ever grant"
        );
        assert!(
            rules
                .iter()
                .all(|rule| rule.scope.as_ref().is_some_and(|s| s.ends_with("/**"))),
            "rules are directory-scoped"
        );
    }

    #[test]
    fn memory_inventory_lists_each_file_once_with_on_load_body_tokens() {
        const INVENTORY_BODY: &str = "remember this exact convention";
        let notes = fixture();
        notes.put(
            "project.md",
            &format!("---\ntags: [project, rust]\n---\n{INVENTORY_BODY}"),
        );

        let found = inventory_dir(&notes.dir, &Mutex::new(notes::TokenCache::default()));

        assert_eq!(found.directory, notes.dir);
        assert_eq!(found.unreadable_files, 0);
        assert_eq!(
            found.notes,
            [MemoryNoteInventory {
                name: "project.md".to_owned(),
                on_load_tokens: estimate_tokens(INVENTORY_BODY),
            }]
        );
    }

    fn remote_context() -> (TempDir, ToolContext) {
        let root = tempdir();
        let workspace = workspace_for_principal("principal");
        let store = Arc::new(LocalDocumentStore::remote(
            StateDir::from_path(root.path().join("state")),
            workspace.binding(),
        ));
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.workspace_session = Some(workspace);
        ctx.local_documents = Some(store);
        crate::tools::native::register_remote(&ctx.registry, &[], FeatureFlags::all()).unwrap();
        (root, ctx)
    }

    async fn dispatch(ctx: &ToolContext, input: &Value) -> crate::types::ToolDoneEvent {
        tool_dispatch::run(
            &ctx.registry,
            None,
            input["command"].as_str().unwrap_or_default().into(),
            MEMORY_TOOL_NAME,
            input,
            ctx,
            Emit::Silent,
        )
        .await
    }

    /// One input every command can run with, so a test can cover them all.
    fn any_command(command: &str) -> Value {
        json!({
            "command": command,
            "path": NOTE_PATH,
            "content": "overwrite",
            "query": NOTE_CONTENT,
            "id": 0,
            "n": 1,
        })
    }

    #[test_case("view")]
    #[test_case("zoom")]
    #[test_case("search")]
    #[test_case("read")]
    #[test_case("write")]
    #[test_case("delete")]
    fn remote_commands_reject_a_store_from_another_principal(command: &str) {
        smol::block_on(async {
            let (_root, mut ctx) = remote_context();
            let store = Arc::clone(ctx.local_documents.as_ref().unwrap());
            store
                .write_memory(store.project_key(), NOTE_PATH, NOTE_CONTENT)
                .unwrap();
            ctx.workspace_session = Some(workspace_for_principal("other"));

            let done = dispatch(&ctx, &any_command(command)).await;

            assert!(done.is_error);
            assert_eq!(done.output.as_text(), WRONG_OWNER);
            assert_eq!(
                store.list_memories(store.project_key()).unwrap()[0].content,
                NOTE_CONTENT
            );
        });
    }

    #[test]
    fn remote_commands_work_by_name_and_expose_no_host_path() {
        smol::block_on(async {
            let (root, ctx) = remote_context();
            let store = ctx.local_documents.as_ref().unwrap();
            for input in [
                json!({ "command": "write", "path": NOTE_PATH, "content": NOTE_CONTENT }),
                json!({ "command": "view" }),
                json!({ "command": "search", "query": NOTE_CONTENT }),
                zoom_input(0, 1),
                json!({ "command": "read", "path": NOTE_PATH }),
                json!({ "command": "delete", "path": NOTE_PATH }),
            ] {
                let done = dispatch(&ctx, &input).await;

                assert!(!done.is_error, "{}", done.output.as_text());
                assert!(done.written_paths().next().is_none());
                let reads = !matches!(input["command"].as_str(), Some("write" | "delete"));
                for text in [done.output.as_text(), done.composed_model_output()] {
                    assert!(!text.contains(root.path().to_str().unwrap()), "{text}");
                    assert!(!reads || text.contains(NOTE_CONTENT), "{text}");
                }
            }
            assert!(store.list_memories(store.project_key()).unwrap().is_empty());
        });
    }

    #[test]
    fn a_remote_note_is_named_by_its_reference() {
        smol::block_on(async {
            let (_root, ctx) = remote_context();
            let store = ctx.local_documents.as_ref().unwrap();
            let reference = store
                .write_memory(store.project_key(), NOTE_PATH, NOTE_CONTENT)
                .unwrap();

            let done = dispatch(&ctx, &json!({ "command": "read", "path": NOTE_PATH })).await;

            let ToolOutput::Memory(MemoryOutput::Notes {
                notes, directory, ..
            }) = &done.output
            else {
                panic!("{FOUND_MSG}");
            };
            assert_eq!(*directory, None);
            assert_eq!(
                notes[0].origin.path(),
                None,
                "a remote note has no host path"
            );
            assert!(done.composed_model_output().contains(reference.as_str()));
        });
    }

    #[test]
    fn a_remote_note_must_be_markdown() {
        smol::block_on(async {
            let (_root, ctx) = remote_context();

            let done = dispatch(
                &ctx,
                &json!({ "command": "write", "path": "note", "content": NOTE_CONTENT }),
            )
            .await;

            assert!(done.is_error);
            assert!(
                done.output.as_text().contains(NAME_NOT_MARKDOWN),
                "{}",
                done.output.as_text()
            );
        });
    }

    #[test_case("write")]
    #[test_case("delete")]
    fn remote_memory_mutation_is_not_an_active_plan_write(command: &str) {
        smol::block_on(async {
            let (_root, mut ctx) = remote_context();
            let store = Arc::clone(ctx.local_documents.as_ref().unwrap());
            store
                .write_memory(store.project_key(), NOTE_PATH, NOTE_CONTENT)
                .unwrap();
            let session = SessionRef::generate();
            let plan = store
                .create_plan(store.project_key(), session.as_str())
                .unwrap();
            ctx.mode = AgentMode::RemotePlan(plan);
            ctx.session_id = Some(session);

            let done = dispatch(&ctx, &any_command(command)).await;

            assert!(done.is_error);
            assert_eq!(done.output.as_text(), PLAN_WRITE_RESTRICTED);
            assert_eq!(
                store.list_memories(store.project_key()).unwrap()[0].content,
                NOTE_CONTENT
            );
        });
    }

    /// Reading remote notes needs no answer whatever the default, as reading
    /// local ones already doesn't. Changing a note still waits for one, and a
    /// rule that names the tool still governs every command.
    #[test_case(DefaultEffect::Prompt, None; "default_prompt")]
    #[test_case(DefaultEffect::Deny, None; "default_deny")]
    #[test_case(DefaultEffect::Allow, Some(Effect::Deny); "explicit_deny")]
    #[test_case(DefaultEffect::Allow, Some(Effect::Ask); "explicit_ask")]
    fn remote_memory_reads_ask_only_when_a_rule_does(
        default: DefaultEffect,
        effect: Option<Effect>,
    ) {
        smol::block_on(async {
            let (root, mut ctx) = remote_context();
            ctx.permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig {
                    default,
                    rules: effect
                        .into_iter()
                        .map(|effect| PermissionRule {
                            tool: ToolKey::native(MEMORY_TOOL_NAME),
                            scope: None,
                            effect,
                        })
                        .collect(),
                    ..Default::default()
                },
                root.path().to_path_buf(),
                Arc::default(),
            ));
            let store = Arc::clone(ctx.local_documents.as_ref().unwrap());
            store
                .write_memory(store.project_key(), NOTE_PATH, NOTE_CONTENT)
                .unwrap();
            for command in COMMANDS {
                let done = dispatch(&ctx, &any_command(command)).await;
                let reads =
                    call(any_command(command)).command.access() == PermissionResourceAccess::Read;
                if reads && effect.is_none() {
                    assert!(!done.is_error, "{command}: {}", done.output.as_text());
                } else {
                    assert!(
                        done.output.as_text().starts_with(PERMISSION_DENIED_PREFIX),
                        "{command}: {}",
                        done.output.as_text()
                    );
                }
            }
            assert_eq!(
                store.list_memories(store.project_key()).unwrap()[0].content,
                NOTE_CONTENT
            );
        });
    }

    #[test_case("view", "", PermissionResourceAccess::Read ; "view_reads")]
    #[test_case("zoom", "0+1", PermissionResourceAccess::Read ; "zoom_reads")]
    #[test_case("search", NOTE_CONTENT, PermissionResourceAccess::Read ; "search_reads")]
    #[test_case("read", NOTE_PATH, PermissionResourceAccess::Read ; "read_reads")]
    #[test_case("write", NOTE_PATH, PermissionResourceAccess::Write ; "write_writes")]
    #[test_case("delete", NOTE_PATH, PermissionResourceAccess::Write ; "delete_writes")]
    fn remote_memory_permission_intent_uses_an_opaque_resource(
        command: &str,
        subject: &str,
        access: PermissionResourceAccess,
    ) {
        let state = tempdir();

        let intent = call_at(any_command(command), state.path()).remote_permission_intent();

        assert_eq!(intent.resources.len(), 1);
        assert_eq!(
            intent.resources[0].kind,
            PermissionResourceKind::Custom {
                name: LOCAL_MEMORY_RESOURCE.to_owned()
            }
        );
        assert_eq!(
            intent.resources[0].value,
            format!("local-memory:{command}:{subject}")
        );
        assert_eq!(intent.resources[0].access, Some(access));
        assert!(
            !intent.resources[0]
                .value
                .contains(state.path().to_str().unwrap())
        );
    }

    #[test]
    fn permission_contract_is_the_registered_native_identity() {
        let (_root, ctx) = remote_context();
        let registered = ctx.registry.get(MEMORY_TOOL_NAME).unwrap();
        assert_eq!(
            registered
                .source
                .permission_identity(MEMORY_TOOL_NAME, None),
            Some((
                PermissionSubject::Native {
                    owner: OWNER.into(),
                    contract: permission_contract().into(),
                },
                PermissionExecutorKind::Native,
            ))
        );
    }
}
