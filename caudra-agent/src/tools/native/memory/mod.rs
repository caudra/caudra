//! `memory`: a project-scoped scratchpad that outlives a session.
//!
//! Notes live under the state directory, keyed by a hash of the project root,
//! and are retrieved by tag rather than by path. The tag index is injected
//! into the system prompt, which is what makes a note findable at all: the
//! model cannot grep a directory it was never told about.

mod notes;
pub mod paths;

use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionScopes, Tool, ToolExecResult,
    ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolContext};
use crate::types::ToolOutput;

pub const DESCRIPTION: &str = "Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions.

- Notes are retrieved by tag; reuse the tags from your system prompt when they fit.
- Save important context before compaction or to build up project knowledge.
- Keep entries concise and current. Delete outdated information.
- The memory `list` and `read` commands report the notes dir; use `file_edit` on `<dir>/<name>` for targeted changes.";

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

/// Both directories are covered: a read may come from the legacy location
/// while writes go to the state directory.
pub fn permission_rules(cwd: &Path) -> Vec<caudra_config::PermissionRule> {
    [paths::legacy_dir(cwd), paths::state_dir(cwd)]
        .into_iter()
        .flatten()
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
const DIR_PREFIX: &str = "dir: ";
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

    /// Only the read paths consult the pre-XDG directory. A write there would
    /// leave the project's notes split across two places.
    fn reads_legacy(self) -> bool {
        matches!(self, Self::List | Self::Read)
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
                .and_then(|cwd| paths::resolve(&cwd, command.reads_legacy())),
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
    pub size: u64,
    pub tag: String,
    /// Notes sharing this tag, for the group header.
    pub tag_count: usize,
}

/// What the `/memory` picker needs, without exposing the note internals.
/// `None` when the notes directory cannot be resolved at all, which is a
/// different failure from having no notes.
pub fn browse(cwd: &Path) -> Option<(PathBuf, Vec<BrowseEntry>, usize)> {
    let dir = paths::resolve(cwd, true)?;
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
                .map(move |(name, size)| BrowseEntry {
                    name,
                    size,
                    tag: tag.clone(),
                    tag_count: count,
                })
        })
        .collect();
    (entries, warnings.len())
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

/// The tag index shown in the system prompt. Absent when there is nothing to
/// say, so a project without notes spends no tokens on the feature.
fn prompt_tag_line(cwd: &Path, cache: &Mutex<notes::TagCache>) -> Option<String> {
    let dir = paths::resolve(cwd, true)?;
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
    Some(format!(
        "\n\nMemory tags (`memory` with `command=\"read\"` and `tags=[...]`): {line}\n"
    ))
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

    fn run(&self, dir: &Path, cache: &Mutex<notes::TagCache>) -> Result<String, String> {
        match self.command {
            Command::List => Ok(self.list(dir, cache)),
            Command::Read if !self.tags.is_empty() => self.read_by_tag(dir, cache),
            Command::Read => self.read_by_path(dir),
            Command::Write => self.write(dir),
            Command::Delete => self.delete(dir),
        }
    }

    fn list(&self, dir: &Path, cache: &Mutex<notes::TagCache>) -> String {
        let wanted = match self.tags.is_empty() {
            true => None,
            false => match notes::tags_for_filter(&self.tags) {
                Ok(pair) => Some(pair),
                Err(error) => return error,
            },
        };
        let (found, read_warnings) = scan(dir, cache);
        let groups = notes::group_by_tag(&found);
        let unreadable = notes::unreadable_warning(&read_warnings);
        if groups.is_empty() && wanted.is_none() {
            return notes::join_parts("\n", &[unreadable, Some(notes::NO_MEMORIES.into())]);
        }
        let warning = wanted.as_ref().and_then(|(_, warning)| warning.clone());
        let matching: Vec<_> = groups
            .iter()
            .filter(|group| {
                wanted
                    .as_ref()
                    .is_none_or(|(want, _)| want.contains(&group.tag))
            })
            .collect();
        if matching.is_empty() {
            return notes::join_parts("\n", &[warning, unreadable, Some(notes::NO_MATCH.into())]);
        }
        let mut lines = Vec::new();
        for group in &matching {
            lines.push(format!("{} ({})", group.tag, group.files.len()));
            for (name, size) in &group.files {
                lines.push(format!("  - {name} ({size} bytes)"));
            }
            lines.push(String::new());
        }
        let mut body = notes::cap(lines.join("\n"), notes::CAP_HINT_FILTER);
        if wanted.is_none() && groups.len() > notes::MAX_TAGS {
            body.push_str(&format!("\n{}", prune_advisory()));
        }
        notes::join_parts("\n", &[warning, unreadable, Some(body)])
    }

    fn read_by_tag(&self, dir: &Path, cache: &Mutex<notes::TagCache>) -> Result<String, String> {
        let (wanted, warning) = notes::tags_for_filter(&self.tags)?;
        let (found, mut read_warnings) = scan(dir, cache);
        let mut entries = Vec::new();
        for note in found
            .iter()
            .filter(|n| n.tags.iter().any(|t| wanted.contains(t)))
        {
            match fs::read_to_string(dir.join(&note.name)) {
                Ok(content) => entries.push(notes::format_entry(&note.name, note.size, &content)),
                Err(error) => read_warnings.push(format!("{}: {error}", note.name)),
            }
        }
        let body = if entries.is_empty() {
            notes::NO_MATCH.to_owned()
        } else {
            let hint = if entries.len() <= 1 {
                notes::CAP_HINT_REWRITE
            } else {
                notes::CAP_HINT_NARROW
            };
            notes::cap(entries.join("\n\n"), hint)
        };
        Ok(notes::join_parts(
            "\n\n",
            &[
                warning,
                notes::unreadable_warning(&read_warnings),
                Some(body),
            ],
        ))
    }

    fn read_by_path(&self, dir: &Path) -> Result<String, String> {
        let path = self.resolved(dir)?;
        let content = fs::read_to_string(&path).map_err(|error| format!("read error: {error}"))?;
        let name = self.path.clone().unwrap_or_default();
        Ok(notes::cap(
            notes::format_entry(&name, content.len() as u64, &content),
            notes::CAP_HINT_REWRITE,
        ))
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

    fn run_all(&self, cache: &Mutex<notes::TagCache>) -> Result<String, String> {
        self.validate()?;
        let dir = self.dir.clone().ok_or(STATE_DIR_UNRESOLVED)?;
        let output = self.run(&dir, cache)?;
        Ok(match self.command {
            // Only the browsing commands report the directory: it is how the
            // model reaches a note with `file_edit`.
            Command::List | Command::Read => format!("{DIR_PREFIX}{}\n\n{output}", dir.display()),
            _ => output,
        })
    }
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
            })
        })
    }

    fn mutation_targets(&self, _ctx: &ToolContext) -> Vec<PathBuf> {
        match (self.command, &self.dir, self.path.as_deref()) {
            (Command::Write | Command::Delete, Some(dir), Some(path)) => {
                paths::safe_resolve(dir, path).into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    fn execute<'a>(self: Box<Self>, _ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            match self.run_all(&TAG_CACHE) {
                Ok(text) => ToolExecResult::from(Ok(ToolOutput::Markdown(text.into()))),
                Err(error) => ToolExecResult::from(Err(format!("error: {error}"))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;
    use serde_json::json;
    use test_case::test_case;

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

    fn run(input: Value, dir: &Path) -> Result<String, String> {
        let call = call_in(input, dir);
        call.validate()?;
        call.run(dir, &Mutex::new(notes::TagCache::default()))
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
        assert!(out.contains("  - a.md"), "{out}");
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
        call_in(input, dir).run_all(&Mutex::new(notes::TagCache::default()))
    }

    #[test]
    fn browsing_commands_report_the_directory_so_notes_can_be_edited() {
        let temp = tempfile::tempdir().unwrap();
        let out = run_all(json!({ "command": "list" }), temp.path()).unwrap();
        assert!(out.starts_with(DIR_PREFIX), "{out}");
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
        assert!(!out.starts_with(DIR_PREFIX), "{out}");
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
    fn a_project_without_notes_contributes_no_prompt_line() {
        let temp = tempfile::tempdir().unwrap();
        assert!(prompt_tag_line(temp.path(), &Mutex::new(notes::TagCache::default())).is_none());
    }
}
