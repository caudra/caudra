//! `skill`: load a named instruction set on demand.
//!
//! Skills are `SKILL.md` files with YAML frontmatter, discovered in the config
//! directory, the user's home, and every project ancestor up to the repository
//! root. Each of those places is a tier list: the first directory that exists
//! wins outright, and the compatibility directories below it are never read.
//! Only the names and descriptions go in the tool description; the body is
//! paid for when the model asks for it.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, OnceLock};

use crate::remote_project_context::RemoteSkill;
use arc_swap::ArcSwap;
use serde_json::Value;

use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, RegisteredTool, Tool, ToolError,
    ToolExecResult, ToolFailure, ToolInvocation, ToolRegistry,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, SKILL_TOOL_NAME, ToolContext, relative_path};
use crate::types::{SkillOutput, ToolOutput};

pub const DESCRIPTION: &str =
    "Load a skill that provides instructions and workflows for specific tasks.";

const SKILL_FILE: &str = "SKILL.md";
const NOT_FOUND: &str = "skill not found: ";
const NO_SKILLS: &str = "No skills available.";
const SKILLS_SUBDIR: &str = "skills";

/// Tier lists, highest priority first. The first directory that exists is the
/// only one read, so a Caudra directory shuts out the compatibility ones.
const PROJECT_SKILL_DIRS: &[&str] = &[
    ".caudra/skills",
    ".claude/skills",
    ".opencode/skills",
    ".agents/skills",
];
const GLOBAL_SKILL_DIRS: &[&str] = &[
    ".claude/skills",
    ".config/opencode/skills",
    ".agents/skills",
];

static NAME_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Name of the skill to load",
};
static PROPERTIES: &[Property] = &[("name", &NAME_PARAM, true, &[])];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: false,
};

/// A skill Caudra ships rather than discovers. `caudra-lua` installs the
/// plugin-authoring skill here at startup: it is generated from the live Lua
/// API docs, which only that crate can render, and `caudra-agent` must not
/// depend on it. The workflow-authoring skill is installed the same way.
pub struct BuiltinSkill {
    pub name: String,
    pub description: String,
    /// Deferred because resolving may write the reference to disk, which is
    /// wasted work for every session that never loads the skill.
    pub resolve: Box<dyn Fn() -> (String, Option<PathBuf>) + Send + Sync>,
}

/// Empty until something installs one, which is exactly how
/// `plugins.skill.plugin_dev = false` turns a builtin skill off.
static BUILTINS: LazyLock<ArcSwap<Vec<Arc<BuiltinSkill>>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Vec::new()));

/// Installing a name twice replaces the earlier skill.
pub fn install_builtin_skill(skill: BuiltinSkill) {
    let skill = Arc::new(skill);
    BUILTINS.rcu(|installed| {
        let mut next: Vec<Arc<BuiltinSkill>> = installed
            .iter()
            .filter(|existing| existing.name != skill.name)
            .cloned()
            .collect();
        next.push(Arc::clone(&skill));
        next
    });
}

fn installed_builtins() -> Arc<Vec<Arc<BuiltinSkill>>> {
    BUILTINS.load_full()
}

/// Where a skill or a search directory came from, so a report can say why one
/// name won and another was never read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    Builtin,
    User,
    Project,
}

impl SkillScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDirState {
    /// Exists and is scanned.
    Selected,
    /// A higher tier in the same group won, so this one is never read.
    Superseded,
    /// Nothing here, and no higher tier claimed the group.
    Missing,
}

impl SkillDirState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::Superseded => "superseded",
            Self::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDirCandidate {
    pub path: PathBuf,
    pub scope: SkillScope,
    pub state: SkillDirState,
}

impl SkillDirCandidate {
    fn is_selected(&self) -> bool {
        self.state == SkillDirState::Selected
    }
}

#[derive(Clone)]
struct Skill {
    name: String,
    description: String,
    location: String,
    scope: SkillScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInventoryEntry {
    pub name: String,
    pub description: String,
    /// The `SKILL.md` path, or `builtin:<name>` for a skill Caudra ships.
    pub location: String,
    pub scope: SkillScope,
}

struct SkillCatalog {
    description: String,
    entries: Vec<SkillInventoryEntry>,
}

pub struct SkillTool {
    dirs: Vec<SkillDirCandidate>,
    remote_skills: Option<Arc<[RemoteSkill]>>,
    /// Built on first use, not at registration: tools register before the
    /// config that decides whether the builtin skill exists. Memoized so the
    /// list the model was given cannot change under it mid-session.
    catalog: OnceLock<SkillCatalog>,
}

impl Default for SkillTool {
    fn default() -> Self {
        Self {
            dirs: skill_dirs(),
            remote_skills: None,
            catalog: OnceLock::new(),
        }
    }
}

impl SkillTool {
    pub fn remote(skills: &[RemoteSkill]) -> Self {
        Self {
            dirs: global_skill_dirs(),
            remote_skills: Some(skills.into()),
            catalog: OnceLock::new(),
        }
    }

    fn catalog(&self) -> &SkillCatalog {
        self.catalog.get_or_init(|| {
            let mut found = discover(&self.dirs, &installed_builtins());
            for skill in self.remote_skills.iter().flat_map(|skills| skills.iter()) {
                found.insert(
                    skill.name.clone(),
                    Skill {
                        name: skill.name.clone(),
                        description: skill.description.clone(),
                        location: skill.source.source_label(),
                        scope: SkillScope::Project,
                    },
                );
            }
            SkillCatalog {
                description: format!("{DESCRIPTION}{}", skill_list(&found)),
                entries: found
                    .values()
                    .map(|skill| SkillInventoryEntry {
                        name: skill.name.clone(),
                        description: skill.description.clone(),
                        location: skill.location.clone(),
                        scope: skill.scope,
                    })
                    .collect(),
            }
        })
    }

    pub fn inventory(&self) -> &[SkillInventoryEntry] {
        &self.catalog().entries
    }
}

pub fn inventory(registry: &ToolRegistry) -> Vec<SkillInventoryEntry> {
    registry
        .get(SKILL_TOOL_NAME)
        .and_then(|registered| {
            registered
                .downcast_ref::<SkillTool>()
                .map(|tool| tool.inventory().to_vec())
        })
        .unwrap_or_default()
}

/// Every candidate directory with the state precedence gave it, so `/skills`
/// and `caudra skills` can show what was skipped rather than leaving a missing
/// skill unexplained.
pub fn directories(registry: &ToolRegistry) -> Vec<SkillDirCandidate> {
    registry
        .get(SKILL_TOOL_NAME)
        .and_then(|registered| {
            registered
                .downcast_ref::<SkillTool>()
                .map(|tool| tool.dirs.clone())
        })
        .unwrap_or_default()
}

/// The body exactly as the model receives it, for `caudra skills <name>`.
pub fn load(registry: &ToolRegistry, name: &str) -> Result<String, String> {
    let registered = registry.get(SKILL_TOOL_NAME);
    let loaded = match registered
        .as_ref()
        .and_then(RegisteredTool::downcast_ref::<SkillTool>)
    {
        Some(tool) => SkillCall {
            name: name.into(),
            dirs: tool.dirs.clone(),
            remote_skills: tool.remote_skills.clone(),
        }
        .load(),
        None => load_from(name, &[], &installed_builtins()),
    };
    loaded
        .map(|skill| skill.model_text())
        .map_err(|error| error.message)
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        SKILL_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(&self.catalog().description)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn tool_kind(&self) -> Option<&str> {
        Some("read")
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ParseError::custom("name is required"))?;
        Ok(Box::new(SkillCall {
            name: name.to_owned(),
            dirs: self.dirs.clone(),
            remote_skills: self.remote_skills.clone(),
        }))
    }
}

struct SkillCall {
    name: String,
    dirs: Vec<SkillDirCandidate>,
    remote_skills: Option<Arc<[RemoteSkill]>>,
}

impl ToolInvocation for SkillCall {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(self.name.clone()))
    }

    fn execute<'a>(mut self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            if ctx.workspace_session.is_some() || ctx.remote_project_context.is_some() {
                self.dirs.retain(|dir| dir.scope == SkillScope::User);
                self.remote_skills = Some(
                    ctx.remote_project_context
                        .as_ref()
                        .map_or_else(|| Arc::from([]), |context| Arc::from(context.skills())),
                );
            }
            match smol::unblock(move || self.load()).await {
                Ok(skill) => ToolExecResult::from(Ok(ToolOutput::Skill(skill))),
                Err(error) => ToolExecResult::failed(error.failure, error.message),
            }
        })
    }
}

impl SkillCall {
    fn load(&self) -> Result<SkillOutput, ToolError> {
        if let Some(skill) = self
            .remote_skills
            .iter()
            .flat_map(|skills| skills.iter())
            .find(|skill| skill.name == self.name)
        {
            return Ok(SkillOutput {
                location: skill.source.source_label(),
                body: skill.content.clone(),
            });
        }
        load_from(&self.name, &self.dirs, &installed_builtins())
    }
}

fn load_from(
    name: &str,
    dirs: &[SkillDirCandidate],
    builtins: &[Arc<BuiltinSkill>],
) -> Result<SkillOutput, ToolError> {
    let discovered = discover(dirs, builtins);
    let Some(skill) = discovered.get(name) else {
        return Err(ToolError::new(
            ToolFailure::NotFound,
            format!("{NOT_FOUND}{name}{}", skill_list(&discovered)),
        ));
    };
    let (body, location) = read_skill(skill, builtins)?;
    Ok(SkillOutput { location, body })
}

/// The model reads this to decide whether to load anything at all, so it
/// carries names and descriptions only.
fn skill_list(skills: &BTreeMap<String, Skill>) -> String {
    if skills.is_empty() {
        return format!("\n\n<available_skills>\n{NO_SKILLS}\n</available_skills>");
    }
    let lines: Vec<String> = skills
        .values()
        .map(|s| format!("- {}: {}", s.name, s.description))
        .collect();
    format!(
        "\n\n<available_skills>\n{}\n</available_skills>",
        lines.join("\n")
    )
}

fn discover(dirs: &[SkillDirCandidate], builtins: &[Arc<BuiltinSkill>]) -> BTreeMap<String, Skill> {
    let mut skills = BTreeMap::new();
    for builtin in builtins {
        skills.insert(
            builtin.name.clone(),
            Skill {
                name: builtin.name.clone(),
                description: builtin.description.clone(),
                location: builtin_location(&builtin.name),
                scope: SkillScope::Builtin,
            },
        );
    }
    for dir in dirs.iter().filter(|dir| dir.is_selected()) {
        scan(&dir.path, dir.scope, &mut skills);
    }
    skills
}

fn builtin_location(name: &str) -> String {
    format!("builtin:{name}")
}

fn scan(dir: &Path, scope: SkillScope, skills: &mut BTreeMap<String, Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path().join(SKILL_FILE);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (frontmatter, body) = parse_frontmatter(&content);
        if body.is_empty() {
            continue;
        }
        let name = frontmatter
            .get("name")
            .cloned()
            .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
        let description = frontmatter.get("description").cloned().unwrap_or_default();
        skills.insert(
            name.clone(),
            Skill {
                name,
                description,
                location: path.to_string_lossy().into_owned(),
                scope,
            },
        );
    }
}

fn read_skill(skill: &Skill, builtins: &[Arc<BuiltinSkill>]) -> Result<(String, String), String> {
    if let Some(builtin) = builtins
        .iter()
        .find(|builtin| skill.location == builtin_location(&builtin.name))
    {
        let (content, reference) = (builtin.resolve)();
        let location = reference.map_or_else(
            || skill.location.clone(),
            |path| path.to_string_lossy().into_owned(),
        );
        return Ok((content, location));
    }
    let content = std::fs::read_to_string(&skill.location)
        .map_err(|e| format!("cannot read {}: {e}", skill.location))?;
    let (_, body) = parse_frontmatter(&content);
    Ok((body, relative_path(&skill.location)))
}

/// Only the scalar `key: value` pairs are read: `name` and `description` are
/// all a skill header is allowed to carry, and a full YAML parse would accept
/// shapes the rest of the code cannot use.
pub(crate) fn parse_frontmatter(content: &str) -> (BTreeMap<String, String>, String) {
    let Some(rest) = content.trim_start().strip_prefix("---\n") else {
        return (BTreeMap::new(), content.trim().to_owned());
    };
    let Some(end) = rest.find("\n---") else {
        return (BTreeMap::new(), content.trim().to_owned());
    };
    let mut fields = BTreeMap::new();
    for line in rest[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']).trim();
        if !value.is_empty() {
            fields.insert(key.trim().to_owned(), value.to_owned());
        }
    }
    let body = rest[end + "\n---".len()..].trim().to_owned();
    (fields, body)
}

/// One tier group. The first directory that exists is selected and everything
/// below it is superseded, existing or not: a Caudra directory is a decision,
/// not a merge.
fn resolve_group(
    candidates: impl IntoIterator<Item = PathBuf>,
    scope: SkillScope,
    dirs: &mut Vec<SkillDirCandidate>,
) {
    let mut claimed = false;
    for path in candidates {
        let state = if claimed {
            SkillDirState::Superseded
        } else if path.is_dir() {
            claimed = true;
            SkillDirState::Selected
        } else {
            SkillDirState::Missing
        };
        dirs.push(SkillDirCandidate { path, scope, state });
    }
}

/// Search order is widest to narrowest, and the map keeps the last write, so a
/// project skill shadows a global one of the same name. The global tier is one
/// group; each project ancestor is a group of its own, so levels still merge
/// and a repo-root skill still shadows a nested one.
fn skill_dirs() -> Vec<SkillDirCandidate> {
    let mut dirs = global_skill_dirs();
    for ancestor in project_ancestors() {
        let level = PROJECT_SKILL_DIRS.iter().map(|rel| ancestor.join(rel));
        resolve_group(level, SkillScope::Project, &mut dirs);
    }
    dirs
}

fn global_skill_dirs() -> Vec<SkillDirCandidate> {
    let mut dirs = Vec::new();
    let config = caudra_storage::paths::config_dir().ok();
    let home = caudra_storage::paths::home();
    let global = caudra_storage::paths::user_config_dir(config.as_deref(), SKILLS_SUBDIR)
        .into_iter()
        .chain(
            home.iter()
                .flat_map(|home| GLOBAL_SKILL_DIRS.iter().map(|rel| home.join(rel))),
        );
    resolve_group(global, SkillScope::User, &mut dirs);

    dirs
}

/// Stops at the repository root: past it the directories belong to an
/// unrelated project, or to the whole filesystem.
fn project_ancestors() -> Vec<PathBuf> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let mut dirs = vec![cwd.clone()];
    if cwd.join(".git").exists() {
        return dirs;
    }
    for parent in cwd.ancestors().skip(1) {
        dirs.push(parent.to_path_buf());
        if parent.join(".git").exists() {
            break;
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_project_context::RemoteAssetIdentity;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, ProjectIdentity, ProjectKey, ResourceId,
        ResourceRevision, SourceTrustAnchor, WorkspacePath,
    };
    use test_case::test_case;

    const BUILTIN_NAME: &str = "caudra-plugin-dev";
    const BUILTIN_BODY: &str = "how to write plugins";
    const BUILTIN_DESC: &str = "author plugins";
    const SHARED_SKILL: &str = "isolation-shared";
    const LOCAL_ONLY_SKILL: &str = "isolation-local-only";
    const LOCAL_BODY: &str = "local project canary";
    const GLOBAL_BODY: &str = "global skill body";
    const REMOTE_BODY: &str = "remote skill body";

    fn remote_skill() -> RemoteSkill {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-source").unwrap(),
            "test-authority",
            "test-workspace",
            "test-generation",
            "test-namespace",
        )
        .unwrap();
        RemoteSkill {
            source: RemoteAssetIdentity {
                principal: AuthenticatedPrincipalId::new(authority.clone(), "test-user").unwrap(),
                project: ProjectIdentity::new(
                    authority.clone(),
                    ProjectKey::new("test-project").unwrap(),
                ),
                authority,
                path: WorkspacePath::new(".caudra/skills/isolation-shared/SKILL.md").unwrap(),
                resource_id: ResourceId::new("skill-resource").unwrap(),
                revision: ResourceRevision::new("skill-revision").unwrap(),
            },
            name: SHARED_SKILL.into(),
            description: REMOTE_BODY.into(),
            content: REMOTE_BODY.into(),
        }
    }

    #[test_case(false; "global_without_remote_override")]
    #[test_case(true; "remote_overrides_global")]
    fn remote_catalog_never_falls_back_to_local_project(include_remote: bool) {
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project_dir = skill_dir(&project, SHARED_SKILL, LOCAL_BODY);
        skill_dir(&project, LOCAL_ONLY_SKILL, LOCAL_BODY);
        let mut global_dir = skill_dir(&global, SHARED_SKILL, GLOBAL_BODY);
        global_dir.scope = SkillScope::User;
        let embedded = SkillTool {
            dirs: vec![global_dir.clone(), project_dir],
            remote_skills: None,
            catalog: OnceLock::new(),
        };
        let skills = if include_remote {
            vec![remote_skill()]
        } else {
            vec![]
        };
        let mut remote = SkillTool::remote(&skills);
        assert!(remote.dirs.iter().all(|dir| dir.scope == SkillScope::User));
        remote.dirs = vec![global_dir];
        let call = |tool: &SkillTool, name: &str| {
            SkillCall {
                name: name.into(),
                dirs: tool.dirs.clone(),
                remote_skills: tool.remote_skills.clone(),
            }
            .load()
            .map(|skill| skill.model_text())
        };
        assert!(call(&embedded, SHARED_SKILL).unwrap().contains(LOCAL_BODY));
        assert!(
            call(&embedded, LOCAL_ONLY_SKILL)
                .unwrap()
                .contains(LOCAL_BODY)
        );
        let expected = if include_remote {
            REMOTE_BODY
        } else {
            GLOBAL_BODY
        };
        let loaded = call(&remote, SHARED_SKILL).unwrap();
        assert!(loaded.contains(expected));
        assert!(!loaded.contains(LOCAL_BODY));
        let error = call(&remote, LOCAL_ONLY_SKILL).unwrap_err().message;
        assert!(error.starts_with(NOT_FOUND));
        assert!(!error.contains(LOCAL_BODY));
        assert!(!remote.catalog().description.contains(LOCAL_ONLY_SKILL));
        assert!(!remote.catalog().description.contains(LOCAL_BODY));
        if include_remote {
            assert!(remote.catalog().description.contains(REMOTE_BODY));
        }
    }

    fn selected(path: PathBuf) -> SkillDirCandidate {
        SkillDirCandidate {
            path,
            scope: SkillScope::Project,
            state: SkillDirState::Selected,
        }
    }

    fn skill_dir(temp: &tempfile::TempDir, name: &str, content: &str) -> SkillDirCandidate {
        let dir = temp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SKILL_FILE), content).unwrap();
        selected(temp.path().to_path_buf())
    }

    fn plugin_dev_skill(reference: Option<PathBuf>) -> BuiltinSkill {
        BuiltinSkill {
            name: BUILTIN_NAME.into(),
            description: BUILTIN_DESC.into(),
            resolve: Box::new(move || (BUILTIN_BODY.into(), reference.clone())),
        }
    }

    #[test]
    fn a_skill_body_is_returned_with_line_numbers() {
        let temp = tempfile::tempdir().unwrap();
        let root = skill_dir(
            &temp,
            "deploy",
            "---\nname: deploy\ndescription: ship it\n---\nfirst\nsecond\n",
        );
        let out = load_from("deploy", &[root], &[]).unwrap().model_text();
        assert!(out.contains("   1 | first"), "{out}");
        assert!(out.contains("   2 | second"), "{out}");
        assert!(!out.contains("description: ship it"), "frontmatter leaked");
    }

    #[test]
    fn the_directory_name_is_the_fallback_skill_name() {
        let temp = tempfile::tempdir().unwrap();
        let root = skill_dir(&temp, "unnamed", "no frontmatter here");
        let found = discover(&[root], &[]);
        assert!(found.contains_key("unnamed"), "{:?}", found.keys());
    }

    #[test]
    fn a_frontmatter_name_wins_over_the_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = skill_dir(&temp, "dirname", "---\nname: realname\n---\nbody\n");
        let found = discover(&[root], &[]);
        assert!(found.contains_key("realname"), "{:?}", found.keys());
        assert!(!found.contains_key("dirname"));
    }

    #[test]
    fn a_body_less_skill_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let root = skill_dir(&temp, "empty", "---\nname: empty\n---\n");
        assert!(discover(&[root], &[]).is_empty());
    }

    /// A later directory shadows an earlier one, which is what makes a project
    /// skill override a global of the same name.
    #[test]
    fn a_later_directory_shadows_an_earlier_one() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let global_root = skill_dir(&global, "deploy", "---\ndescription: global\n---\nbody\n");
        let project_root = skill_dir(&project, "deploy", "---\ndescription: local\n---\nbody\n");
        let found = discover(&[global_root, project_root], &[]);
        assert_eq!(found["deploy"].description, "local");
    }

    #[test]
    fn an_unknown_skill_lists_what_is_available() {
        let temp = tempfile::tempdir().unwrap();
        let root = skill_dir(&temp, "deploy", "---\ndescription: ship it\n---\nbody\n");
        let error = load_from("nope", &[root], &[]).unwrap_err();
        assert_eq!(error.failure, ToolFailure::NotFound);
        let error = error.message;
        assert!(error.starts_with(NOT_FOUND), "{error}");
        assert!(error.contains("- deploy: ship it"), "{error}");
    }

    #[test]
    fn no_skills_at_all_still_produces_a_usable_list() {
        assert!(skill_list(&BTreeMap::new()).contains(NO_SKILLS));
    }

    #[test]
    fn the_listing_is_sorted_so_the_description_is_reproducible() {
        let temp = tempfile::tempdir().unwrap();
        skill_dir(&temp, "zulu", "---\ndescription: z\n---\nbody\n");
        skill_dir(&temp, "alpha", "---\ndescription: a\n---\nbody\n");
        let root = skill_dir(&temp, "mike", "---\ndescription: m\n---\nbody\n");
        let listing = skill_list(&discover(&[root], &[]));
        let alpha = listing.find("alpha").unwrap();
        let mike = listing.find("mike").unwrap();
        let zulu = listing.find("zulu").unwrap();
        assert!(alpha < mike && mike < zulu, "{listing}");
    }

    #[test]
    fn structured_inventory_is_the_same_memoized_catalog_the_model_sees() {
        let temp = tempfile::tempdir().unwrap();
        let root = skill_dir(
            &temp,
            "deploy",
            "---\ndescription: ship safely\n---\nbody\n",
        );
        let tool = SkillTool {
            dirs: vec![root],
            remote_skills: None,
            catalog: OnceLock::new(),
        };

        let first = tool.inventory().to_vec();
        skill_dir(&temp, "later", "---\ndescription: added later\n---\nbody\n");

        assert_eq!(tool.inventory(), first);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name, "deploy");
        assert_eq!(first[0].description, "ship safely");
        assert!(tool.catalog().description.contains("- deploy: ship safely"));
        assert!(!tool.catalog().description.contains("later"));
    }

    #[test]
    fn an_uninstalled_builtin_is_simply_absent() {
        assert!(discover(&[], &[]).is_empty());
        let error = load_from(BUILTIN_NAME, &[], &[]).unwrap_err().message;
        assert!(error.starts_with(NOT_FOUND), "{error}");
    }

    #[test]
    fn an_installed_builtin_is_listed_and_loadable() {
        let builtins = [Arc::new(plugin_dev_skill(None))];
        assert_eq!(
            discover(&[], &builtins)[BUILTIN_NAME].description,
            BUILTIN_DESC
        );
        let out = load_from(BUILTIN_NAME, &[], &builtins).unwrap();
        assert_eq!(out.body, BUILTIN_BODY);
        assert_eq!(out.location, builtin_location(BUILTIN_NAME));
    }

    /// Every builtin is its own entry, and each loads its own body rather
    /// than the first one installed.
    #[test]
    fn several_builtins_are_listed_and_each_loads_its_own_body() {
        const OTHER_NAME: &str = "caudra-other";
        const OTHER_BODY: &str = "how to do the other thing";
        let builtins = [
            Arc::new(plugin_dev_skill(None)),
            Arc::new(BuiltinSkill {
                name: OTHER_NAME.into(),
                description: BUILTIN_DESC.into(),
                resolve: Box::new(|| (OTHER_BODY.into(), None)),
            }),
        ];
        let found = discover(&[], &builtins);
        assert!(found.contains_key(BUILTIN_NAME) && found.contains_key(OTHER_NAME));
        let out = load_from(OTHER_NAME, &[], &builtins).unwrap();
        assert_eq!(out.body, OTHER_BODY);
    }

    /// The plugin-dev skill spills the API reference to disk and reports that
    /// path so the model can read it directly.
    #[test]
    fn a_builtin_that_spills_a_reference_reports_its_path() {
        let reference = PathBuf::from("/state/docs/lua-api.md");
        let builtin = plugin_dev_skill(Some(reference.clone()));
        let out = load_from(BUILTIN_NAME, &[], &[Arc::new(builtin)]).unwrap();
        assert_eq!(out.location, reference.to_string_lossy());
    }

    #[test]
    fn a_disk_skill_of_the_same_name_does_not_take_the_builtin_path() {
        const DISK_BODY: &str = "real file content";
        let temp = tempfile::tempdir().unwrap();
        let root = skill_dir(
            &temp,
            "ondisk",
            &format!("---\nname: {BUILTIN_NAME}\n---\n{DISK_BODY}\n"),
        );
        let builtin = plugin_dev_skill(None);
        let out = load_from(BUILTIN_NAME, &[root], &[Arc::new(builtin)]).unwrap();
        assert_eq!(out.body, DISK_BODY);
    }

    const TIERS: &[&str] = &["caudra", "claude", "opencode", "agents"];

    fn tier_group(temp: &tempfile::TempDir, existing: &[&str]) -> Vec<SkillDirCandidate> {
        for name in existing {
            std::fs::create_dir_all(temp.path().join(name)).unwrap();
        }
        let mut dirs = Vec::new();
        resolve_group(
            TIERS.iter().map(|tier| temp.path().join(tier)),
            SkillScope::User,
            &mut dirs,
        );
        dirs
    }

    fn states(dirs: &[SkillDirCandidate]) -> Vec<SkillDirState> {
        dirs.iter().map(|dir| dir.state).collect()
    }

    use SkillDirState::{Missing, Selected, Superseded};

    #[test_case(
        &["caudra"], &[Selected, Superseded, Superseded, Superseded]
        ; "the_caudra_directory_shuts_out_the_rest"
    )]
    #[test_case(
        &["claude", "opencode"], &[Missing, Selected, Superseded, Superseded]
        ; "the_first_existing_directory_wins"
    )]
    #[test_case(
        &["agents"], &[Missing, Missing, Missing, Selected]
        ; "the_last_tier_is_still_reachable"
    )]
    #[test_case(&[], &[Missing, Missing, Missing, Missing] ; "nothing_to_select")]
    fn a_tier_group_selects_the_first_existing_directory(
        existing: &[&str],
        expected: &[SkillDirState],
    ) {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(states(&tier_group(&temp, existing)), expected);
    }

    #[test]
    fn a_regular_file_never_claims_a_tier() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(TIERS[0]), "not a directory").unwrap();
        assert_eq!(
            states(&tier_group(&temp, &["claude"])),
            &[Missing, Selected, Superseded, Superseded]
        );
    }

    /// The whole point of the tiers: an empty Caudra directory is an answer,
    /// not a reason to fall through to somebody else's skills.
    #[test]
    fn an_empty_selected_directory_still_shuts_out_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let compat = temp.path().join(TIERS[1]).join("deploy");
        std::fs::create_dir_all(&compat).unwrap();
        std::fs::write(compat.join(SKILL_FILE), "---\nname: deploy\n---\nbody\n").unwrap();

        let dirs = tier_group(&temp, &["caudra", "claude"]);
        assert_eq!(
            states(&dirs),
            &[Selected, Superseded, Superseded, Superseded]
        );
        assert!(discover(&dirs, &[]).is_empty());
    }

    #[test]
    fn every_entry_carries_where_it_came_from() {
        let temp = tempfile::tempdir().unwrap();
        let mut root = skill_dir(&temp, "deploy", "---\ndescription: ship it\n---\nbody\n");
        root.scope = SkillScope::User;
        let tool = SkillTool {
            dirs: vec![root],
            remote_skills: None,
            catalog: OnceLock::new(),
        };

        let entry = &tool.inventory()[0];
        assert_eq!(entry.scope, SkillScope::User);
        assert_eq!(
            entry.location,
            temp.path()
                .join("deploy")
                .join(SKILL_FILE)
                .to_string_lossy()
        );
    }

    #[test]
    fn the_builtin_skill_is_attributed_to_caudra() {
        let builtin = plugin_dev_skill(None);
        let found = discover(&[], &[Arc::new(builtin)]);
        assert_eq!(found[BUILTIN_NAME].scope, SkillScope::Builtin);
        assert_eq!(found[BUILTIN_NAME].location, builtin_location(BUILTIN_NAME));
    }

    #[test_case("---\nname: a\n---\nbody", Some("a"), "body" ; "well_formed")]
    #[test_case("no frontmatter", None, "no frontmatter" ; "absent")]
    #[test_case("---\nname: a\nbody without close", None, "---\nname: a\nbody without close" ; "unterminated")]
    #[test_case("---\nname: \"quoted\"\n---\nb", Some("quoted"), "b" ; "quotes_stripped")]
    #[test_case("---\nname:\n---\nb", None, "b" ; "empty_value_dropped")]
    fn frontmatter_parsing(input: &str, name: Option<&str>, body: &str) {
        let (fields, parsed) = parse_frontmatter(input);
        assert_eq!(fields.get("name").map(String::as_str), name);
        assert_eq!(parsed, body);
    }
}
