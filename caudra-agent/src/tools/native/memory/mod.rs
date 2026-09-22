//! `memory`: a project-scoped scratchpad that outlives a session.
//!
//! Notes live under the state directory, keyed by a hash of the project root,
//! and are retrieved by tag rather than by path. The tag index is injected
//! into the system prompt, which is what makes a note findable at all: the
//! model cannot grep a directory it was never told about.

mod notes;
pub mod paths;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use caudra_providers::estimate_tokens;
use caudra_storage::local_documents::{LocalDocument, LocalDocumentStore};
use caudra_workspace::{LocalDocumentRef, MemoryRef};
use serde_json::Value;

use crate::permissions::{PermissionResource, PermissionResourceKind, PermissionRisk};
use crate::tools::native::local_document::scoped_store;
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionIntent, PermissionScopes, Tool,
    ToolEffect, ToolExecResult, ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolContext};
use crate::types::{
    MemoryNote, MemoryNoteEntry, MemoryOrigin, MemoryOutput, MemoryTagGroup, ToolOutput,
};

pub const DESCRIPTION: &str = "Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions.

- Notes are retrieved by tag; reuse the tags from your system prompt when they fit.
- Save important context before compaction or to build up project knowledge.
- Keep entries concise and current. Delete outdated information.
- Concision comes from dropping facts, never from dropping spaces or running words together. A note that cannot be read costs more than the tokens it saved.
- Embedded `list` and `read` report the notes dir for `file_edit`. Remote sessions return opaque memory references for `local_document_read`, `local_document_write`, or `local_document_apply_patch`; no client host path is exposed.";

pub const TOOL_USAGE: &str =
    "- Proactively save non-obvious project gotchas and architecture decisions to **memory**.";

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

pub fn permission_rules(cwd: &Path) -> Vec<caudra_config::PermissionRule> {
    paths::state_dir(cwd)
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

const COMMANDS: &[&str] = &["list", "read", "write", "delete"];
/// What a remote note with no name of its own is called. The store allows it;
/// a card and a text rendering both need something to print.
const UNNAMED_REMOTE_NOTE: &str = "memory";
const PROMPT_TAG_PREFIX: &str =
    "\n\nMemory tags (`memory` with `command=\"read\"` and `tags=[...]`): ";
const STATE_DIR_UNRESOLVED: &str = "cannot resolve state dir";

static COMMAND_PARAM: ParamSchema = ParamSchema::Enum {
    variants: COMMANDS,
    description: "- `list [tags]`: tag-grouped index, no bodies.
- `read path|tags`: one body (path) or collated bodies (tags).
- `write path tags content`: create or overwrite a note.
- `delete path`",
};
static PATH_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Relative path, e.g. 'architecture.md'.",
};
static CONTENT_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Body for write (frontmatter added automatically).",
};
static TAG_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "",
};
static TAGS_PARAM: ParamSchema = ParamSchema::Array {
    items: &TAG_PARAM,
    description: "snake_case tags. Filter for list/read; assigned on write (defaults to filename stem).",
};
static PROPERTIES: &[Property] = &[
    ("command", &COMMAND_PARAM, true, &[]),
    ("path", &PATH_PARAM, false, &[]),
    ("content", &CONTENT_PARAM, false, &[]),
    ("tags", &TAGS_PARAM, false, &[]),
];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: false,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Command {
    List,
    Read,
    Write,
    Delete,
}

impl Command {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "list" => Some(Self::List),
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Read => "read",
            Self::Write => "write",
            Self::Delete => "delete",
        }
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
        Ok(Box::new(MemoryCall {
            command,
            // Resolved once: permission scoping, mutation targets, and the run
            // itself must agree on where the notes are, and three separate
            // `current_dir` calls need not.
            dir: std::env::current_dir()
                .ok()
                .and_then(|cwd| paths::state_dir(&cwd)),
            path: string_field(&input, "path"),
            content: string_field(&input, "content"),
            tags: input
                .get("tags")
                .and_then(Value::as_array)
                .map(|tags| {
                    tags.iter()
                        .filter_map(|tag| tag.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        }))
    }
}

fn string_field(input: &Value, name: &str) -> Option<String> {
    input
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// One note as the `/memory` picker shows it: a note carrying several tags
/// appears once per tag, which is how the picker groups them.
pub struct BrowseEntry {
    pub name: String,
    pub tokens: u32,
    pub tag: String,
    /// Notes sharing this tag, for the group header.
    pub tag_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNoteInventory {
    pub name: String,
    pub on_load_tokens: u32,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryInventory {
    pub directory: PathBuf,
    pub notes: Vec<MemoryNoteInventory>,
    pub unreadable_files: usize,
}

pub fn inventory(cwd: &Path) -> Option<MemoryInventory> {
    let directory = paths::state_dir(cwd)?;
    Some(inventory_dir(&directory, &TAG_CACHE))
}

fn inventory_dir(dir: &Path, cache: &Mutex<notes::TagCache>) -> MemoryInventory {
    let (notes, warnings) = scan(dir, cache);
    MemoryInventory {
        directory: dir.to_path_buf(),
        notes: notes
            .into_iter()
            .map(|note| MemoryNoteInventory {
                name: note.name,
                on_load_tokens: note.tokens,
                tags: note.tags,
            })
            .collect(),
        unreadable_files: warnings.len(),
    }
}

/// What the `/memory` picker needs, without exposing the note internals.
/// `None` when the notes directory cannot be resolved at all, which is a
/// different failure from having no notes.
pub fn browse(cwd: &Path) -> Option<(PathBuf, Vec<BrowseEntry>, usize)> {
    let dir = paths::state_dir(cwd)?;
    let (entries, unreadable) = browse_dir(&dir, &TAG_CACHE);
    Some((dir, entries, unreadable))
}

fn browse_dir(dir: &Path, cache: &Mutex<notes::TagCache>) -> (Vec<BrowseEntry>, usize) {
    let (found, warnings) = scan(dir, cache);
    let entries = notes::group_by_tag(&found)
        .into_iter()
        .flat_map(|group| {
            let count = group.files.len();
            let tag = group.tag;
            group
                .files
                .into_iter()
                .map(move |(name, tokens)| BrowseEntry {
                    name,
                    tokens,
                    tag: tag.clone(),
                    tag_count: count,
                })
        })
        .collect();
    (entries, warnings.len())
}

pub fn browse_store(store: &LocalDocumentStore) -> Result<Vec<(MemoryRef, BrowseEntry)>, String> {
    let documents = store
        .list_memories(store.project_key())
        .map_err(|error| error.to_string())?;
    let mut groups = BTreeMap::<String, Vec<(MemoryRef, String, u32)>>::new();
    for document in documents {
        let LocalDocumentRef::Memory(reference) = &document.reference else {
            continue;
        };
        let name = document.name.as_deref().unwrap_or_default();
        let label = format!("{name} [{}]", reference.as_str());
        let mut tags = document_tags(&document);
        if tags.is_empty() {
            tags.push(name.to_owned());
        }
        for tag in tags {
            groups.entry(tag).or_default().push((
                reference.clone(),
                label.clone(),
                estimate_tokens(&document.content),
            ));
        }
    }
    Ok(groups
        .into_iter()
        .flat_map(|(tag, files)| {
            let tag_count = files.len();
            files.into_iter().map(move |(reference, name, tokens)| {
                (
                    reference,
                    BrowseEntry {
                        name,
                        tokens,
                        tag: tag.clone(),
                        tag_count,
                    },
                )
            })
        })
        .collect())
}

/// Shared by the prompt's tag line and the tool's own scans. The prompt is
/// rebuilt every turn and would otherwise re-read every note each time; the
/// cache keys on size and mtime, so a note the tool just wrote is never stale.
static TAG_CACHE: LazyLock<Mutex<notes::TagCache>> = LazyLock::new(Mutex::default);

/// The prompt is built from the process working directory, the same ambient
/// value the rest of the tools layer resolves paths against.
pub fn prompt_tag_line_for_cwd() -> Option<String> {
    prompt_tag_line(&std::env::current_dir().ok()?, &TAG_CACHE)
}

pub fn prompt_tag_line_for_store(store: &LocalDocumentStore) -> Option<String> {
    let documents = store.list_memories(store.project_key()).ok()?;
    let mut tags = documents
        .iter()
        .flat_map(|document| {
            let (frontmatter, _) = notes::parse_frontmatter(&document.content);
            frontmatter
                .as_ref()
                .and_then(|value| value.get("tags"))
                .and_then(serde_yaml::Value::as_sequence)
                .into_iter()
                .flatten()
                .filter_map(serde_yaml::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    tags.sort();
    tags.dedup();
    (!tags.is_empty()).then(|| format!("{PROMPT_TAG_PREFIX}{}\n", tags.join(", ")))
}

/// The tag index shown in the system prompt. Absent when there is nothing to
/// say, so a project without notes spends no tokens on the feature.
fn prompt_tag_line(cwd: &Path, cache: &Mutex<notes::TagCache>) -> Option<String> {
    let dir = paths::state_dir(cwd)?;
    let (found, warnings) = scan(&dir, cache);
    let groups = notes::group_by_tag(&found);
    if groups.is_empty() && warnings.is_empty() {
        return None;
    }
    let mut line = groups
        .iter()
        .take(notes::MAX_TAGS)
        .map(|group| group.tag.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if groups.len() > notes::MAX_TAGS {
        line.push_str(&format!(
            " ... ({} tags omitted; use `list` to see all)",
            groups.len() - notes::MAX_TAGS
        ));
    }
    if !warnings.is_empty() {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&format!("(unreadable: {})", warnings.len()));
    }
    Some(format!("{PROMPT_TAG_PREFIX}{line}\n"))
}

pub(crate) fn prompt_tag_line_range(system: &str) -> Option<Range<usize>> {
    let start = system.rfind(PROMPT_TAG_PREFIX)?;
    let content_start = start + PROMPT_TAG_PREFIX.len();
    let end = content_start + system[content_start..].find('\n')? + 1;
    Some(start..end)
}

fn scan(dir: &Path, cache: &Mutex<notes::TagCache>) -> (Vec<notes::Note>, Vec<String>) {
    match cache.lock() {
        Ok(mut cache) => notes::scan(dir, &mut cache),
        // A poisoned cache is not worth failing a read over; it only ever
        // holds derived data.
        Err(_) => notes::scan(dir, &mut notes::TagCache::default()),
    }
}

struct MemoryCall {
    command: Command,
    dir: Option<PathBuf>,
    path: Option<String>,
    content: Option<String>,
    tags: Vec<String>,
}

impl MemoryCall {
    /// Rejects combinations the schema cannot express: which of `path`,
    /// `tags`, and `content` a command needs depends on the command.
    fn validate(&self) -> Result<(), String> {
        let has_path = self.path.is_some();
        let has_tags = !self.tags.is_empty();
        match self.command {
            Command::Read if has_path && has_tags => {
                Err("provide 'path' or 'tags', not both".into())
            }
            Command::Read if !has_path && !has_tags => {
                Err("'path' or 'tags' is required for read".into())
            }
            Command::Write if !has_path => Err("'path' is required for write".into()),
            Command::Write if self.content.is_none() => {
                Err("'content' is required for write".into())
            }
            Command::Delete if !has_path => Err("'path' is required for delete".into()),
            _ => Ok(()),
        }
    }

    fn run(&self, dir: &Path, cache: &Mutex<notes::TagCache>) -> Result<Answer, String> {
        match self.command {
            Command::List => Ok(Answer::Browse(self.list(dir, cache))),
            Command::Read if !self.tags.is_empty() => {
                self.read_by_tag(dir, cache).map(Answer::Browse)
            }
            Command::Read => self.read_by_path(dir).map(Answer::Browse),
            Command::Write => self.write(dir).map(Answer::Receipt),
            Command::Delete => self.delete(dir).map(Answer::Receipt),
        }
    }

    fn list(&self, dir: &Path, cache: &Mutex<notes::TagCache>) -> MemoryOutput {
        let directory = Some(dir.display().to_string());
        let wanted = match self.tags.is_empty() {
            true => None,
            false => match notes::tags_for_filter(&self.tags) {
                Ok(pair) => Some(pair),
                Err(error) => return empty_index(directory, vec![error]),
            },
        };
        let (found, read_warnings) = scan(dir, cache);
        let all = notes::group_by_tag(&found);
        let mut notices: Vec<String> = wanted
            .as_ref()
            .and_then(|(_, warning)| warning.clone())
            .into_iter()
            .chain(notes::unreadable_warning(&read_warnings))
            .collect();
        let groups: Vec<MemoryTagGroup> = all
            .iter()
            .filter(|group| {
                wanted
                    .as_ref()
                    .is_none_or(|(want, _)| want.contains(&group.tag))
            })
            .map(|group| MemoryTagGroup {
                tag: group.tag.clone(),
                notes: group
                    .files
                    .iter()
                    .map(|(name, tokens)| MemoryNoteEntry {
                        name: name.clone(),
                        tokens: *tokens,
                        origin: file_origin(dir, name),
                    })
                    .collect(),
            })
            .collect();
        if groups.is_empty() {
            notices.push(match wanted.is_none() {
                true => notes::NO_MEMORIES.to_owned(),
                false => notes::NO_MATCH.to_owned(),
            });
        }
        if wanted.is_none() && all.len() > notes::MAX_TAGS {
            notices.push(prune_advisory());
        }
        MemoryOutput::Index {
            directory,
            groups,
            notices,
        }
    }

    fn read_by_tag(
        &self,
        dir: &Path,
        cache: &Mutex<notes::TagCache>,
    ) -> Result<MemoryOutput, String> {
        let (wanted, warning) = notes::tags_for_filter(&self.tags)?;
        let (found, mut read_warnings) = scan(dir, cache);
        let mut read = Vec::new();
        for note in found
            .iter()
            .filter(|n| n.tags.iter().any(|t| wanted.contains(t)))
        {
            match fs::read_to_string(dir.join(&note.name)) {
                Ok(content) => read.push(local_note(dir, &note.name, &content)),
                Err(error) => read_warnings.push(format!("{}: {error}", note.name)),
            }
        }
        let mut notices: Vec<String> = warning
            .into_iter()
            .chain(notes::unreadable_warning(&read_warnings))
            .collect();
        if read.is_empty() {
            notices.push(notes::NO_MATCH.to_owned());
        }
        Ok(MemoryOutput::Notes {
            directory: Some(dir.display().to_string()),
            notes: read,
            notices,
        })
    }

    fn read_by_path(&self, dir: &Path) -> Result<MemoryOutput, String> {
        let path = self.resolved(dir)?;
        let content = fs::read_to_string(&path).map_err(|error| format!("read error: {error}"))?;
        let name = self.path.clone().unwrap_or_default();
        Ok(MemoryOutput::Notes {
            directory: Some(dir.display().to_string()),
            notes: Vec::from([local_note(dir, &name, &content)]),
            notices: Vec::new(),
        })
    }

    fn write(&self, dir: &Path) -> Result<String, String> {
        let path = self.resolved(dir)?;
        let content = self.content.clone().unwrap_or_default();
        if let Some(error) = notes::write_size_error(&content) {
            return Err(error);
        }
        let (tags, note) = notes::tags_for_write(&self.tags)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("write error: {error}"))?;
        }
        fs::write(
            &path,
            format!("{}{content}", notes::encode_frontmatter(&tags)),
        )
        .map_err(|error| format!("write error: {error}"))?;
        let listed = if tags.is_empty() {
            "none".to_owned()
        } else {
            tags.join(", ")
        };
        let suffix = note.map_or(String::new(), |note| format!("; {note}"));
        Ok(format!(
            "wrote {} (tags: {listed}){suffix}",
            self.path.clone().unwrap_or_default()
        ))
    }

    fn delete(&self, dir: &Path) -> Result<String, String> {
        let path = self.resolved(dir)?;
        let name = self.path.clone().unwrap_or_default();
        if !path.exists() {
            return Err(format!("'{name}' does not exist"));
        }
        fs::remove_file(&path).map_err(|error| format!("delete error: {error}"))?;
        Ok(format!("deleted {name}"))
    }

    fn resolved(&self, dir: &Path) -> Result<PathBuf, String> {
        paths::safe_resolve(dir, self.path.as_deref().unwrap_or_default())
    }

    fn run_all(&self, cache: &Mutex<notes::TagCache>) -> Result<Answer, String> {
        self.validate()?;
        let dir = self.dir.clone().ok_or(STATE_DIR_UNRESOLVED)?;
        self.run(&dir, cache)
    }
}

/// What a command answered with. The two browsing commands return something a
/// card draws; the two that change a note return the receipt for having done
/// it, which is all there is to say.
enum Answer {
    Browse(MemoryOutput),
    Receipt(String),
}

fn file_origin(dir: &Path, name: &str) -> MemoryOrigin {
    MemoryOrigin::File {
        path: dir.join(name).to_string_lossy().into_owned(),
    }
}

/// A note as the card and the model both read it: the body without its
/// frontmatter, and the tags that frontmatter carried stated once.
fn local_note(dir: &Path, name: &str, content: &str) -> MemoryNote {
    let (frontmatter, body) = notes::parse_frontmatter(content);
    MemoryNote {
        name: name.to_owned(),
        tokens: estimate_tokens(body),
        tags: notes::tags_from_frontmatter(frontmatter.as_ref()).unwrap_or_default(),
        origin: file_origin(dir, name),
        body: body.to_owned(),
    }
}

fn empty_index(directory: Option<String>, notices: Vec<String>) -> MemoryOutput {
    MemoryOutput::Index {
        directory,
        groups: Vec::new(),
        notices,
    }
}

/// How much of a browse the model is charged for. The card draws from the
/// structure and has its own row budget, so the byte cap is only ever about
/// what a result costs the context window.
fn capped_model_text(output: &MemoryOutput) -> String {
    let hint = match output {
        MemoryOutput::Notes { notes, .. } if notes.len() > 1 => notes::CAP_HINT_NARROW,
        MemoryOutput::Notes { .. } => notes::CAP_HINT_REWRITE,
        MemoryOutput::Index { .. } => notes::CAP_HINT_FILTER,
    };
    notes::cap(output.as_display_text(), hint)
}

fn prune_advisory() -> String {
    format!(
        "Consider removing or consolidating stale memories to stay under {} tags.",
        notes::MAX_TAGS
    )
}

impl ToolInvocation for MemoryCall {
    fn start_header(&self) -> HeaderFuture {
        let detail = self
            .path
            .clone()
            .or_else(|| (!self.tags.is_empty()).then(|| self.tags.join(",")));
        HeaderFuture::Ready(HeaderResult::plain(match detail {
            Some(detail) => format!("{} {detail}", self.command.as_str()),
            None => self.command.as_str().to_owned(),
        }))
    }

    fn permission_scopes(&self) -> crate::tools::registry::BoxFuture<'_, Option<PermissionScopes>> {
        Box::pin(async move {
            let dir = self.dir.clone()?;
            // A path-scoped call approves one file; a tag-scoped one has to
            // cover the whole directory because it cannot name its targets yet.
            let scope = match self.path.as_deref() {
                Some(path) => paths::safe_resolve(&dir, path).unwrap_or(dir),
                None => dir.join("**"),
            };
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
    ) -> crate::tools::registry::BoxFuture<'a, Result<Option<PermissionIntent>, String>> {
        Box::pin(async move {
            if ctx.workspace_session.is_none() {
                return Ok(None);
            }
            scoped_store(ctx)?;
            Ok(Some(self.remote_permission_intent()))
        })
    }

    /// Browsing a scratchpad is a read; only the two commands that touch a
    /// note carry the registered mutating effect.
    fn call_effect(&self, registered: ToolEffect) -> ToolEffect {
        match self.command {
            Command::List | Command::Read => ToolEffect::ReadOnly,
            Command::Write | Command::Delete => registered,
        }
    }

    fn mutation_targets(&self, ctx: &ToolContext) -> Vec<PathBuf> {
        if ctx.workspace_session.is_some() {
            return Vec::new();
        }
        match (self.command, &self.dir, self.path.as_deref()) {
            (Command::Write | Command::Delete, Some(dir), Some(path)) => {
                paths::safe_resolve(dir, path).into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let answer = match self.run_for_context(_ctx) {
                Ok(answer) => answer,
                Err(error) => return ToolExecResult::from(Err(format!("error: {error}"))),
            };
            let receipt = match answer {
                // A browse is drawn from its structure, and the model reads the
                // one rendering of it that fits the context window.
                Answer::Browse(output) => {
                    let text = capped_model_text(&output);
                    return ToolExecResult::from(Ok(ToolOutput::Memory(output)))
                        .with_model_output(Some(text));
                }
                Answer::Receipt(receipt) => receipt,
            };
            // A write's reply is a receipt. The note is what the reader came
            // for, and the model already has it, so only the receipt goes back.
            let note = (self.command == Command::Write)
                .then_some(self.content.as_deref())
                .flatten()
                .filter(|content| !content.trim().is_empty());
            match note {
                Some(note) => ToolExecResult::from(Ok(ToolOutput::Markdown(note.into())))
                    .with_model_output(Some(receipt)),
                None => ToolExecResult::from(Ok(ToolOutput::Markdown(receipt.into()))),
            }
        })
    }
}

impl MemoryCall {
    fn remote_permission_intent(&self) -> PermissionIntent {
        let detail = self
            .path
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| self.tags.join(","));
        let scope = format!("local-memory:{}:{detail}", self.command.as_str());
        PermissionIntent::new(
            PermissionScopes::single(scope.clone()),
            vec![PermissionResource {
                kind: PermissionResourceKind::Custom {
                    name: "local_memory".to_owned(),
                },
                value: scope,
                access: None,
                protected: false,
                requires_prompt: false,
                attributes: BTreeMap::new(),
            }],
            PermissionRisk::Low,
        )
    }

    fn run_for_context(&self, ctx: &ToolContext) -> Result<Answer, String> {
        if ctx.workspace_session.is_none() {
            return self.run_all(&TAG_CACHE);
        }
        self.validate()?;
        let store = scoped_store(ctx)?;
        let project = store.project_key();
        let documents = store
            .list_memories(project)
            .map_err(|error| error.to_string())?;
        match self.command {
            Command::List => Ok(Answer::Browse(self.remote_list(&documents))),
            Command::Read if !self.tags.is_empty() => {
                self.remote_read_by_tag(&documents).map(Answer::Browse)
            }
            Command::Read => self.remote_read_by_path(&documents).map(Answer::Browse),
            Command::Write => self.remote_write(store, project).map(Answer::Receipt),
            Command::Delete => self.remote_delete(store, project).map(Answer::Receipt),
        }
    }

    /// Grouped by tag exactly as a local list is, so the same card draws both.
    /// A remote note has no path, so its reference is what names it.
    fn remote_list(&self, documents: &[LocalDocument]) -> MemoryOutput {
        let wanted = notes::tags_for_filter(&self.tags)
            .ok()
            .map(|(tags, _)| tags);
        let mut by_tag = BTreeMap::<String, Vec<MemoryNoteEntry>>::new();
        for document in documents {
            let note = remote_note(document);
            if wanted
                .as_ref()
                .is_some_and(|wanted| !note.tags.iter().any(|tag| wanted.contains(tag)))
            {
                continue;
            }
            for tag in &note.tags {
                by_tag
                    .entry(tag.clone())
                    .or_default()
                    .push(MemoryNoteEntry {
                        name: note.name.clone(),
                        tokens: note.tokens,
                        origin: note.origin.clone(),
                    });
            }
        }
        let mut groups: Vec<MemoryTagGroup> = by_tag
            .into_iter()
            .map(|(tag, notes)| MemoryTagGroup { tag, notes })
            .collect();
        groups.sort_by(|a, b| b.notes.len().cmp(&a.notes.len()).then(a.tag.cmp(&b.tag)));
        let notices = match groups.is_empty() {
            true => Vec::from([notes::NO_MEMORIES.to_owned()]),
            false => Vec::new(),
        };
        MemoryOutput::Index {
            directory: None,
            groups,
            notices,
        }
    }

    fn remote_read_by_tag(&self, documents: &[LocalDocument]) -> Result<MemoryOutput, String> {
        let (wanted, warning) = notes::tags_for_filter(&self.tags)?;
        let read: Vec<MemoryNote> = documents
            .iter()
            .map(remote_note)
            .filter(|note| note.tags.iter().any(|tag| wanted.contains(tag)))
            .collect();
        let mut notices: Vec<String> = warning.into_iter().collect();
        if read.is_empty() {
            notices.push(notes::NO_MATCH.to_owned());
        }
        Ok(MemoryOutput::Notes {
            directory: None,
            notes: read,
            notices,
        })
    }

    fn remote_read_by_path(&self, documents: &[LocalDocument]) -> Result<MemoryOutput, String> {
        let name = self.path.as_deref().unwrap_or_default();
        let document = documents
            .iter()
            .find(|document| document.name.as_deref() == Some(name))
            .ok_or_else(|| format!("'{name}' does not exist"))?;
        Ok(MemoryOutput::Notes {
            directory: None,
            notes: Vec::from([remote_note(document)]),
            notices: Vec::new(),
        })
    }

    fn remote_write(
        &self,
        store: &LocalDocumentStore,
        project: &caudra_workspace::ProjectKey,
    ) -> Result<String, String> {
        let content = self.content.as_deref().unwrap_or_default();
        if let Some(error) = notes::write_size_error(content) {
            return Err(error);
        }
        let (tags, note) = notes::tags_for_write(&self.tags)?;
        let body = format!("{}{content}", notes::encode_frontmatter(&tags));
        let reference = store
            .write_memory(project, self.path.as_deref().unwrap_or_default(), &body)
            .map_err(|error| error.to_string())?;
        let suffix = note.map_or(String::new(), |note| format!("; {note}"));
        Ok(format!("wrote memory_ref {}{suffix}", reference.as_str()))
    }

    fn remote_delete(
        &self,
        store: &LocalDocumentStore,
        project: &caudra_workspace::ProjectKey,
    ) -> Result<String, String> {
        let reference = store
            .delete_memory(project, self.path.as_deref().unwrap_or_default())
            .map_err(|error| error.to_string())?;
        Ok(format!("deleted memory_ref {}", reference.as_str()))
    }
}

fn document_tags(document: &LocalDocument) -> Vec<String> {
    let (frontmatter, _) = notes::parse_frontmatter(&document.content);
    frontmatter
        .as_ref()
        .and_then(|value| value.get("tags"))
        .and_then(serde_yaml::Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(serde_yaml::Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// The remote twin of [`local_note`]: the same note, named by the reference and
/// revision that are the only way to reach it, and with the frontmatter split
/// off so both sides report the body's own cost.
fn remote_note(document: &LocalDocument) -> MemoryNote {
    let (_, body) = notes::parse_frontmatter(&document.content);
    MemoryNote {
        name: document
            .name
            .clone()
            .unwrap_or_else(|| UNNAMED_REMOTE_NOTE.to_owned()),
        tokens: estimate_tokens(body),
        tags: document_tags(document),
        origin: MemoryOrigin::Document {
            reference: reference_id(&document.reference).to_owned(),
            revision: document.revision.as_str().to_owned(),
        },
        body: body.to_owned(),
    }
}

fn reference_id(reference: &LocalDocumentRef) -> &str {
    match reference {
        LocalDocumentRef::Memory(reference) => reference.as_str(),
        LocalDocumentRef::Plan(reference) => reference.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use std::slice;

    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;
    use crate::types::MEMORY_DIRECTORY_LABEL;
    use caudra_providers::token_label;
    use caudra_storage::local_documents::DocumentRevision;
    use caudra_workspace::MemoryRef;
    use serde_json::json;
    use test_case::test_case;

    const REGISTERED_EFFECT: ToolEffect = ToolEffect::Mutating;

    fn call(input: Value) -> Box<dyn ToolInvocation> {
        MemoryTool.parse(&input).expect("valid input")
    }

    /// Points a parsed call at a temporary directory, which is the only way to
    /// exercise it without writing into the developer's real notes.
    fn call_in(input: Value, dir: &Path) -> MemoryCall {
        let command = Command::parse(input["command"].as_str().expect("command")).expect("known");
        MemoryCall {
            command,
            dir: Some(dir.to_path_buf()),
            path: string_field(&input, "path"),
            content: string_field(&input, "content"),
            tags: input["tags"]
                .as_array()
                .map(|tags| {
                    tags.iter()
                        .filter_map(|tag| tag.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Every command answers as text somewhere: a browse through the one
    /// rendering both the model and the card's copy read, a mutation through
    /// its receipt.
    fn text(answer: Answer) -> String {
        match answer {
            Answer::Browse(output) => capped_model_text(&output),
            Answer::Receipt(receipt) => receipt,
        }
    }

    fn run(input: Value, dir: &Path) -> Result<String, String> {
        let call = call_in(input, dir);
        call.validate()?;
        call.run(dir, &Mutex::new(notes::TagCache::default()))
            .map(text)
    }

    fn browse(input: Value, dir: &Path) -> MemoryOutput {
        match call_in(input, dir)
            .run(dir, &Mutex::new(notes::TagCache::default()))
            .expect("a browse that succeeded")
        {
            Answer::Browse(output) => output,
            Answer::Receipt(receipt) => panic!("a browse answers with notes, got {receipt}"),
        }
    }

    #[test]
    fn a_write_stores_frontmatter_and_the_body() {
        let temp = tempfile::tempdir().unwrap();
        let out = run(
            json!({ "command": "write", "path": "arch.md", "content": "body", "tags": ["arch"] }),
            temp.path(),
        )
        .unwrap();
        assert_eq!(out, "wrote arch.md (tags: arch)");
        let stored = fs::read_to_string(temp.path().join("arch.md")).unwrap();
        assert!(stored.starts_with("---\n"), "{stored}");
        assert!(stored.ends_with("body"), "{stored}");
    }

    #[test]
    fn a_write_creates_the_directory() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("missing");
        run(
            json!({ "command": "write", "path": "a.md", "content": "x", "tags": ["t"] }),
            &dir,
        )
        .unwrap();
        assert!(dir.join("a.md").exists());
    }

    #[test]
    fn a_write_without_tags_is_stored_untagged() {
        let temp = tempfile::tempdir().unwrap();
        let out = run(
            json!({ "command": "write", "path": "a.md", "content": "x" }),
            temp.path(),
        )
        .unwrap();
        assert!(out.contains("tags: none"), "{out}");
    }

    #[test]
    fn a_note_reads_back_by_path() {
        let temp = tempfile::tempdir().unwrap();
        run(
            json!({ "command": "write", "path": "a.md", "content": "the body", "tags": ["t"] }),
            temp.path(),
        )
        .unwrap();
        let out = run(json!({ "command": "read", "path": "a.md" }), temp.path()).unwrap();
        assert!(out.contains("the body"), "{out}");
        assert!(out.contains("[t]"), "{out}");
    }

    #[test]
    fn a_note_reads_back_by_tag() {
        let temp = tempfile::tempdir().unwrap();
        run(
            json!({ "command": "write", "path": "a.md", "content": "tagged body", "tags": ["arch"] }),
            temp.path(),
        )
        .unwrap();
        let out = run(json!({ "command": "read", "tags": ["arch"] }), temp.path()).unwrap();
        assert!(out.contains("tagged body"), "{out}");
    }

    /// The write path normalizes tags, so a read must normalize its filter the
    /// same way or a note becomes unreachable by the tag it was filed under.
    #[test]
    fn a_tag_filter_is_normalized_like_the_stored_tag() {
        let temp = tempfile::tempdir().unwrap();
        run(
            json!({ "command": "write", "path": "a.md", "content": "found", "tags": ["Build-System"] }),
            temp.path(),
        )
        .unwrap();
        let out = run(
            json!({ "command": "read", "tags": ["build system"] }),
            temp.path(),
        )
        .unwrap();
        assert!(out.contains("found"), "{out}");
    }

    #[test]
    fn a_tag_that_matches_nothing_says_so() {
        let temp = tempfile::tempdir().unwrap();
        run(
            json!({ "command": "write", "path": "a.md", "content": "x", "tags": ["arch"] }),
            temp.path(),
        )
        .unwrap();
        let out = run(
            json!({ "command": "read", "tags": ["absent"] }),
            temp.path(),
        )
        .unwrap();
        assert!(out.contains(notes::NO_MATCH), "{out}");
    }

    #[test]
    fn an_empty_directory_lists_as_empty() {
        let temp = tempfile::tempdir().unwrap();
        let out = run(json!({ "command": "list" }), temp.path()).unwrap();
        assert!(out.contains(notes::NO_MEMORIES), "{out}");
    }

    #[test]
    fn a_list_groups_notes_under_their_tags() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a.md", "b.md"] {
            run(
                json!({ "command": "write", "path": name, "content": "x", "tags": ["shared"] }),
                temp.path(),
            )
            .unwrap();
        }
        let out = run(json!({ "command": "list" }), temp.path()).unwrap();
        assert!(out.contains("shared (2)"), "{out}");
        assert!(
            out.contains(&format!("  - a.md ({})", token_label(1))),
            "{out}"
        );
    }

    #[test]
    fn a_delete_removes_the_file() {
        let temp = tempfile::tempdir().unwrap();
        run(
            json!({ "command": "write", "path": "a.md", "content": "x", "tags": ["t"] }),
            temp.path(),
        )
        .unwrap();
        let out = run(json!({ "command": "delete", "path": "a.md" }), temp.path()).unwrap();
        assert_eq!(out, "deleted a.md");
        assert!(!temp.path().join("a.md").exists());
    }

    #[test]
    fn deleting_a_missing_note_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let error = run(
            json!({ "command": "delete", "path": "gone.md" }),
            temp.path(),
        )
        .unwrap_err();
        assert!(error.contains("does not exist"), "{error}");
    }

    #[test]
    fn an_oversized_write_never_reaches_disk() {
        let temp = tempfile::tempdir().unwrap();
        let huge = "x".repeat(notes::MAX_FILE_BYTES + 1);
        let error = run(
            json!({ "command": "write", "path": "a.md", "content": huge, "tags": ["t"] }),
            temp.path(),
        )
        .unwrap_err();
        assert!(error.contains("exceeds"), "{error}");
        assert!(!temp.path().join("a.md").exists());
    }

    #[test]
    fn a_traversing_path_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let error = run(
            json!({ "command": "write", "path": "../escape.md", "content": "x" }),
            temp.path(),
        )
        .unwrap_err();
        assert_eq!(error, paths::PATH_TRAVERSAL);
    }

    #[test_case(json!({ "command": "read" }), "'path' or 'tags' is required" ; "read needs a selector")]
    #[test_case(json!({ "command": "read", "path": "a.md", "tags": ["t"] }), "not both" ; "read takes one selector")]
    #[test_case(json!({ "command": "write", "content": "x" }), "'path' is required" ; "write needs a path")]
    #[test_case(json!({ "command": "write", "path": "a.md" }), "'content' is required" ; "write needs content")]
    #[test_case(json!({ "command": "delete" }), "'path' is required" ; "delete needs a path")]
    fn an_incoherent_call_is_rejected(input: Value, expected: &str) {
        let error = call_in(input, Path::new("/memories"))
            .validate()
            .unwrap_err();
        assert!(error.contains(expected), "{error}");
    }

    #[test]
    fn an_unknown_command_is_rejected_at_parse_time() {
        let Err(error) = MemoryTool.parse(&json!({ "command": "purge" })) else {
            panic!("an unknown command must not parse");
        };
        assert!(error.to_string().contains("purge"), "{error}");
    }

    /// The model routinely sends a bare string where the schema says array;
    /// the shared validator wraps it, and this pins that the tool relies on it.
    #[test]
    fn a_single_tag_string_is_accepted() {
        let parsed = MemoryTool
            .parse(&json!({ "command": "read", "tags": "solo" }))
            .unwrap();
        let HeaderResult::Plain(header) = smol::block_on(parsed.start_header()) else {
            panic!("memory headers are plain");
        };
        assert_eq!(header, "read solo");
    }

    #[test_case(json!({ "command": "list" }), "list" ; "bare command")]
    #[test_case(json!({ "command": "read", "path": "a.md" }), "read a.md" ; "with a path")]
    #[test_case(json!({ "command": "list", "tags": ["a", "b"] }), "list a,b" ; "with tags")]
    fn the_header_describes_the_call(input: Value, expected: &str) {
        let HeaderResult::Plain(header) = smol::block_on(call(input).start_header()) else {
            panic!("memory headers are plain");
        };
        assert_eq!(header, expected);
    }

    /// `browse` resolves the real notes directory from the cwd, which tests
    /// must not touch.
    fn browse_in(dir: &Path) -> (Vec<BrowseEntry>, usize) {
        browse_dir(dir, &Mutex::new(notes::TagCache::default()))
    }

    fn run_all(input: Value, dir: &Path) -> Result<String, String> {
        call_in(input, dir)
            .run_all(&Mutex::new(notes::TagCache::default()))
            .map(text)
    }

    #[test]
    fn browsing_commands_report_the_directory_so_notes_can_be_edited() {
        let temp = tempfile::tempdir().unwrap();
        let out = run_all(json!({ "command": "list" }), temp.path()).unwrap();
        assert!(out.starts_with(MEMORY_DIRECTORY_LABEL), "{out}");
        assert!(out.contains(&temp.path().display().to_string()), "{out}");
    }

    #[test]
    fn a_write_does_not_report_the_directory() {
        let temp = tempfile::tempdir().unwrap();
        let out = run_all(
            json!({ "command": "write", "path": "a.md", "content": "x" }),
            temp.path(),
        )
        .unwrap();
        assert!(!out.starts_with(MEMORY_DIRECTORY_LABEL), "{out}");
    }

    const BODY_MSG: &str = "the reader sees the note, the model sees the receipt";
    const BROWSE_MSG: &str = "a browse answers with one structure both sides read";
    const NOTE_BODY: &str = "# Session picker\n\nThe picker merges **two** sources.";

    fn execute_in(input: Value, dir: &Path) -> ToolExecResult {
        let call = Box::new(call_in(input, dir));
        smol::block_on(call.execute(&stub_ctx(&AgentMode::Build)))
    }

    fn markdown(result: ToolExecResult) -> String {
        match result.output.expect("a call that succeeded") {
            ToolOutput::Markdown(text) => text.text,
            other => panic!("{BODY_MSG}, got {other:?}"),
        }
    }

    /// A write's reply is a receipt, so rendering it is rendering nothing. The
    /// note is what the reader came for, and the model wrote it and does not
    /// need it back.
    #[test]
    fn a_write_renders_the_note_and_replies_with_the_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let out = execute_in(
            json!({ "command": "write", "path": "a.md", "content": NOTE_BODY, "tags": ["ui"] }),
            temp.path(),
        );
        assert_eq!(out.model_output.as_deref(), Some("wrote a.md (tags: ui)"));
        assert_eq!(markdown(out), NOTE_BODY, "{BODY_MSG}");
    }

    /// Nothing to render is not a reason to render nothing: an empty note
    /// would leave the row with no body at all, so the receipt stands in.
    #[test]
    fn a_blank_note_falls_back_to_its_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let out = execute_in(
            json!({ "command": "write", "path": "a.md", "content": "  \n " }),
            temp.path(),
        );
        assert_eq!(out.model_output, None, "{BODY_MSG}");
        assert!(markdown(out).starts_with("wrote a.md"), "{BODY_MSG}");
    }

    /// Browsing has one answer, so both sides read it: the reader gets the
    /// structure the card draws, and the model gets the one rendering of that
    /// same structure.
    #[test_case("list" ; "a list")]
    #[test_case("read" ; "a read")]
    fn a_browsing_call_shows_the_model_what_the_reader_sees(command: &str) {
        let temp = tempfile::tempdir().unwrap();
        run(
            json!({ "command": "write", "path": "a.md", "content": NOTE_BODY }),
            temp.path(),
        )
        .unwrap();
        let out = execute_in(json!({ "command": command, "path": "a.md" }), temp.path());
        let ToolOutput::Memory(output) = out.output.expect("a browse that succeeded") else {
            panic!("{BROWSE_MSG}");
        };
        assert_eq!(
            out.model_output.as_deref(),
            Some(output.as_display_text().as_str()),
            "{BROWSE_MSG}"
        );
        assert!(
            output.as_display_text().contains(MEMORY_DIRECTORY_LABEL),
            "{BROWSE_MSG}"
        );
    }

    #[test]
    fn a_failing_call_is_reported_as_a_tool_error() {
        let out = smol::block_on(
            call(json!({ "command": "read" })).execute(&stub_ctx(&AgentMode::Build)),
        );
        assert!(out.is_error);
        assert!(out.output.unwrap_err().starts_with("error: "));
    }

    /// Write and delete declare their target so the permission layer can see
    /// the file before it changes.
    #[test]
    fn a_mutating_call_declares_the_file_it_touches() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let write = call_in(
            json!({ "command": "write", "path": "a.md", "content": "x" }),
            temp.path(),
        );
        assert_eq!(write.mutation_targets(&ctx), vec![temp.path().join("a.md")]);
        let list = call_in(json!({ "command": "list" }), temp.path());
        assert!(list.mutation_targets(&ctx).is_empty());
    }

    /// The registration is mutating so a write is gated; browsing has to
    /// report itself as the read it is or plan mode refuses it.
    #[test_case("list", ToolEffect::ReadOnly ; "list_is_a_read")]
    #[test_case("read", ToolEffect::ReadOnly ; "read_is_a_read")]
    #[test_case("write", REGISTERED_EFFECT ; "write_keeps_the_registered_effect")]
    #[test_case("delete", REGISTERED_EFFECT ; "delete_keeps_the_registered_effect")]
    fn the_call_effect_follows_the_command(command: &str, expected: ToolEffect) {
        let parsed = call(json!({ "command": command, "path": "a.md", "content": "x" }));
        assert_eq!(parsed.call_effect(REGISTERED_EFFECT), expected);
    }

    #[test]
    fn a_tag_scoped_call_asks_for_the_whole_directory() {
        let temp = tempfile::tempdir().unwrap();
        let call = call_in(json!({ "command": "read", "tags": ["t"] }), temp.path());
        let scopes = smol::block_on(call.permission_scopes()).unwrap();
        assert_eq!(
            scopes.scopes,
            vec![temp.path().join("**").to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn a_path_scoped_call_asks_only_for_that_file() {
        let temp = tempfile::tempdir().unwrap();
        let call = call_in(json!({ "command": "read", "path": "a.md" }), temp.path());
        let scopes = smol::block_on(call.permission_scopes()).unwrap();
        assert_eq!(
            scopes.scopes,
            vec![temp.path().join("a.md").to_string_lossy().into_owned()]
        );
    }

    /// Every note-touching tool needs a rule for every directory notes can
    /// live in, or the model gets a prompt for its own scratchpad.
    #[test]
    fn permission_rules_cover_each_policy_tool() {
        let temp = tempfile::tempdir().unwrap();
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
    fn browsing_lists_each_note_once_per_tag() {
        let temp = tempfile::tempdir().unwrap();
        run(
            json!({ "command": "write", "path": "a.md", "content": "x", "tags": ["one", "two"] }),
            temp.path(),
        )
        .unwrap();
        let (entries, warnings) = browse_in(temp.path());
        assert_eq!(warnings, 0);
        let tags: Vec<_> = entries.iter().map(|entry| entry.tag.as_str()).collect();
        assert_eq!(tags, vec!["one", "two"]);
        assert!(entries.iter().all(|entry| entry.name == "a.md"));
    }

    #[test]
    fn browsing_reports_how_many_notes_share_each_tag() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a.md", "b.md"] {
            run(
                json!({ "command": "write", "path": name, "content": "x", "tags": ["shared"] }),
                temp.path(),
            )
            .unwrap();
        }
        let (entries, _) = browse_in(temp.path());
        assert!(entries.iter().all(|entry| entry.tag_count == 2));
    }

    #[test]
    fn browsing_an_empty_directory_yields_no_entries() {
        let temp = tempfile::tempdir().unwrap();
        assert!(browse_in(temp.path()).0.is_empty());
    }

    #[test]
    fn memory_inventory_lists_each_file_once_with_on_load_body_tokens() {
        const BODY: &str = "remember this exact convention";

        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("project.md"),
            format!("---\ntags: [project, rust]\n---\n{BODY}"),
        )
        .unwrap();
        let found = inventory_dir(temp.path(), &Mutex::new(notes::TagCache::default()));

        assert_eq!(found.directory, temp.path());
        assert_eq!(found.unreadable_files, 0);
        assert_eq!(found.notes.len(), 1);
        assert_eq!(found.notes[0].name, "project.md");
        assert_eq!(found.notes[0].tags, vec!["project", "rust"]);
        assert_eq!(
            found.notes[0].on_load_tokens,
            caudra_providers::estimate_tokens(BODY)
        );
    }

    #[test]
    fn a_project_without_notes_contributes_no_prompt_line() {
        let temp = tempfile::tempdir().unwrap();
        assert!(prompt_tag_line(temp.path(), &Mutex::new(notes::TagCache::default())).is_none());
    }

    const REMOTE_NAME: &str = "architecture.md";
    const REMOTE_BODY: &str = "keep this";
    const REMOTE_TAG: &str = "rust";
    const CLIENT_STATE: &str = "/secret/client/state";

    fn remote_document() -> (MemoryRef, LocalDocument) {
        let reference = MemoryRef::new(format!("memory-{}", "a".repeat(64))).expect("memory ref");
        let document = LocalDocument {
            reference: LocalDocumentRef::Memory(reference.clone()),
            name: Some(REMOTE_NAME.into()),
            content: format!("---\ntags: [{REMOTE_TAG}]\n---\n{REMOTE_BODY}"),
            revision: DocumentRevision::new("b".repeat(64)).expect("revision"),
        };
        (reference, document)
    }

    #[test]
    fn remote_memory_browsing_returns_refs_without_a_directory() {
        let (reference, document) = remote_document();
        let state = Path::new(CLIENT_STATE);

        let listed = call_in(json!({"command": "list"}), state)
            .remote_list(slice::from_ref(&document))
            .as_display_text();
        let read = call_in(json!({"command": "read", "path": REMOTE_NAME}), state)
            .remote_read_by_path(slice::from_ref(&document))
            .expect("the named note")
            .as_display_text();

        assert!(listed.contains(reference.as_str()), "{listed}");
        assert!(read.contains(reference.as_str()), "{read}");
        assert!(!listed.contains(CLIENT_STATE), "{listed}");
        assert!(!read.contains(CLIENT_STATE), "{read}");
        assert!(!read.contains(MEMORY_DIRECTORY_LABEL), "{read}");
    }

    /// The reported divergence: a remote read handed the model the raw
    /// frontmatter and a token count that included it, while a local read
    /// stripped both. One structure, so one shape.
    #[test]
    fn a_remote_note_reads_like_a_local_one() {
        let temp = tempfile::tempdir().unwrap();
        let (_, document) = remote_document();
        fs::write(temp.path().join(REMOTE_NAME), &document.content).unwrap();

        let local = local_note(temp.path(), REMOTE_NAME, &document.content);
        let remote = remote_note(&document);

        assert_eq!(remote.headline(), local.headline());
        assert_eq!(remote.body, REMOTE_BODY);
        assert_eq!(remote.tags, vec![REMOTE_TAG]);
        assert_eq!(remote.origin.path(), None, "a remote note has no host path");
        assert_eq!(
            local.origin.path(),
            Some(temp.path().join(REMOTE_NAME).to_string_lossy().as_ref())
        );
    }

    #[test]
    fn a_read_carries_the_note_the_card_draws() {
        let temp = tempfile::tempdir().unwrap();
        run(
            json!({ "command": "write", "path": "a.md", "content": NOTE_BODY, "tags": ["ui"] }),
            temp.path(),
        )
        .unwrap();

        let MemoryOutput::Notes { notes, notices, .. } =
            browse(json!({ "command": "read", "path": "a.md" }), temp.path())
        else {
            panic!("{BROWSE_MSG}");
        };

        assert!(notices.is_empty());
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].body, NOTE_BODY, "the body keeps its markdown");
        assert_eq!(notes[0].tags, vec!["ui"], "and states its tags once");
    }

    /// The row a click opens. A note the model can reach by name is a file on
    /// this host, and the card needs the whole path to open it.
    #[test]
    fn an_indexed_note_names_the_file_a_click_opens() {
        let temp = tempfile::tempdir().unwrap();
        run(
            json!({ "command": "write", "path": "a.md", "content": "x", "tags": ["shared"] }),
            temp.path(),
        )
        .unwrap();

        let MemoryOutput::Index { groups, .. } = browse(json!({ "command": "list" }), temp.path())
        else {
            panic!("{BROWSE_MSG}");
        };

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tag, "shared");
        assert_eq!(
            groups[0].notes[0].origin.path(),
            Some(temp.path().join("a.md").to_string_lossy().as_ref())
        );
    }

    /// Nothing found is a notice rather than a body, so a card draws one dim
    /// line instead of a note that is not there.
    #[test_case(json!({ "command": "list" }), notes::NO_MEMORIES ; "an_empty_scratchpad")]
    #[test_case(json!({ "command": "read", "tags": ["absent"] }), notes::NO_MATCH ; "a_filter_reaching_nothing")]
    fn an_empty_browse_says_so_in_a_notice(input: Value, expected: &str) {
        let temp = tempfile::tempdir().unwrap();
        let output = browse(input, temp.path());

        assert!(output.is_empty());
        assert_eq!(output.notices(), [expected.to_owned()]);
    }

    #[test]
    fn remote_memory_permission_intent_uses_an_opaque_resource() {
        let call = call_in(
            json!({"command": "write", "path": "architecture.md", "content": "note"}),
            Path::new("/secret/client/state"),
        );

        let intent = call.remote_permission_intent();

        assert_eq!(intent.resources.len(), 1);
        assert_eq!(
            intent.resources[0].kind,
            PermissionResourceKind::Custom {
                name: "local_memory".to_owned()
            }
        );
        assert_eq!(
            intent.resources[0].value,
            "local-memory:write:architecture.md"
        );
        assert!(!intent.resources[0].value.contains("/secret/client/state"));
    }
}
