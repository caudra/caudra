use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use caudra_config::{DefaultEffect, Effect, PermissionRule, PermissionsConfig, ToolKey};
use caudra_workflow::meta::MAX_SOURCE_BYTES;
use caudra_workflow::{WORKFLOW_ABI_VERSION, WORKFLOW_LANGUAGE_VERSION, WorkflowMeta, parse_meta};
use caudra_workspace::{
    AuthenticatedPrincipalId, AuthorityIdentity, CollectionRevision, ProjectAsset,
    ProjectAssetKind, ProjectAssetTrust, ProjectIdentity, ResourceId, ResourceRevision,
    WorkspacePath, WorkspaceSession,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::agent::{INSTRUCTION_FILES, LOCAL_INSTRUCTION_FILE};
use crate::command::{CustomCommand, parse_remote_command};
use crate::tools::native::skill::parse_frontmatter;

const MANIFEST_VERSION: &str = "project-assets.v1";
const MAX_ASSETS: usize = 256;
const MAX_CONTEXTS: usize = 16;
const MAX_TOTAL_BYTES: u64 = 1024 * 1024;
const MAX_INSTRUCTION_BYTES: u32 = 64 * 1024;
const MAX_COMMAND_BYTES: u32 = 64 * 1024;
const MAX_SKILL_BYTES: u32 = 128 * 1024;
const MAX_PERMISSION_BYTES: u32 = 128 * 1024;
const SHA256_HEX_LEN: usize = 64;
const MAX_NAMED_SKIPS: usize = 4;
const SKIPPED_ASSETS_WARNING: &str = "Remote project assets Caudra does not recognise were skipped";

static REMOTE_CONTEXT_LOADER: LazyLock<RemoteProjectContextLoader> =
    LazyLock::new(RemoteProjectContextLoader::new);

/// Ranked after every file in [`INSTRUCTION_FILES`], and matched by full path
/// only, so it governs the project root wherever a workspace puts it.
const CAUDRA_INSTRUCTIONS: &str = ".caudra/instructions";
const SKILL_ROOTS: &[&str] = &[".caudra", ".claude", ".opencode", ".agents"];
const COMMAND_ROOTS: &[&str] = &[".caudra", ".claude", ".opencode"];

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RemoteProjectContextError {
    #[error("remote project assets are unavailable")]
    Unavailable,
    #[error("remote project asset manifest is invalid")]
    InvalidManifest,
    #[error("remote project asset declaration is invalid for {0}")]
    InvalidAsset(String),
    #[error("remote project asset changed while it was being loaded: {0}")]
    StaleAsset(String),
    #[error("remote project asset exceeds its content limit: {0}")]
    AssetTooLarge(String),
    #[error("remote project asset is invalid: {0}")]
    InvalidContent(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteAssetIdentity {
    pub authority: AuthorityIdentity,
    pub principal: AuthenticatedPrincipalId,
    pub project: ProjectIdentity,
    pub path: WorkspacePath,
    pub resource_id: ResourceId,
    pub revision: ResourceRevision,
}

impl RemoteAssetIdentity {
    pub fn source_label(&self) -> String {
        format!(
            "{} (resource {}, revision {})",
            self.path,
            self.resource_id.as_str(),
            self.revision.as_str()
        )
    }

    pub fn trust_key(
        &self,
    ) -> Result<caudra_workspace::ProjectAssetTrustKey, RemoteProjectContextError> {
        caudra_workspace::ProjectAssetTrustKey::from_parts(
            self.authority.clone(),
            self.principal.clone(),
            self.project.clone(),
            self.path.clone(),
            self.resource_id.clone(),
            self.revision.clone(),
        )
        .map_err(|_| RemoteProjectContextError::InvalidManifest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInstruction {
    pub source: RemoteAssetIdentity,
    pub content: String,
}

impl RemoteInstruction {
    /// True for a workspace `AGENTS.local.md`, which is rendered at local scope
    /// so the model reads it the same way it reads the host's own overlay.
    pub fn is_personal_overlay(&self) -> bool {
        ranked_instruction(self.source.path.as_str()).is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSkill {
    pub source: RemoteAssetIdentity,
    pub name: String,
    pub description: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteWorkflow {
    pub source: RemoteAssetIdentity,
    pub meta: WorkflowMeta,
    pub content: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemotePermissionDeclarations {
    pub restrictive_rules: Vec<PermissionRule>,
    pub allow_rules: Vec<PermissionRule>,
    pub restrictive_defaults: HashMap<ToolKey, DefaultEffect>,
    pub allow_defaults: HashMap<ToolKey, DefaultEffect>,
    pub default: Option<DefaultEffect>,
}

impl RemotePermissionDeclarations {
    pub fn apply(&self, config: &mut PermissionsConfig, allows_trusted: bool) {
        config.rules.extend(self.restrictive_rules.iter().cloned());
        config.tool_defaults.extend(
            self.restrictive_defaults
                .iter()
                .map(|(key, value)| (key.clone(), *value)),
        );
        if let Some(default) = self
            .default
            .filter(|effect| *effect != DefaultEffect::Allow)
        {
            config.default = default;
        }
        if allows_trusted {
            config.rules.extend(self.allow_rules.iter().cloned());
            config.tool_defaults.extend(
                self.allow_defaults
                    .iter()
                    .map(|(key, value)| (key.clone(), *value)),
            );
            if self.default == Some(DefaultEffect::Allow) {
                config.default = DefaultEffect::Allow;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePermissionAsset {
    pub source: RemoteAssetIdentity,
    pub digest: String,
    pub declarations: RemotePermissionDeclarations,
}

#[derive(Debug, Clone)]
pub struct RemoteProjectContext {
    manifest_revision: CollectionRevision,
    instructions: Vec<RemoteInstruction>,
    commands: Vec<CustomCommand>,
    skills: Vec<RemoteSkill>,
    workflows: Vec<RemoteWorkflow>,
    permissions: Option<RemotePermissionAsset>,
    skipped: Vec<WorkspacePath>,
}

impl RemoteProjectContext {
    pub fn manifest_revision(&self) -> &CollectionRevision {
        &self.manifest_revision
    }

    pub fn instructions(&self) -> &[RemoteInstruction] {
        &self.instructions
    }

    pub fn commands(&self) -> &[CustomCommand] {
        &self.commands
    }

    pub fn skills(&self) -> &[RemoteSkill] {
        &self.skills
    }

    pub fn workflows(&self) -> &[RemoteWorkflow] {
        &self.workflows
    }

    pub fn permissions(&self) -> Option<&RemotePermissionAsset> {
        self.permissions.as_ref()
    }

    /// Declared assets Caudra could not account for and therefore never read.
    /// Worth saying out loud once: the project meant them to apply.
    pub fn skipped(&self) -> &[WorkspacePath] {
        &self.skipped
    }

    pub fn skipped_warning(&self) -> Option<String> {
        if self.skipped.is_empty() {
            return None;
        }
        let named: Vec<_> = self
            .skipped
            .iter()
            .take(MAX_NAMED_SKIPS)
            .map(WorkspacePath::as_str)
            .collect();
        let rest = self.skipped.len() - named.len();
        let mut warning = format!("{SKIPPED_ASSETS_WARNING}: {}", named.join(", "));
        if rest > 0 {
            warning.push_str(&format!(" and {rest} more"));
        }
        Some(warning)
    }

    pub fn applicable_instructions(&self, path: &WorkspacePath) -> Vec<&RemoteInstruction> {
        let target_dir = if path.is_root() {
            "."
        } else {
            path.as_str()
                .rsplit_once('/')
                .map_or(".", |(parent, _)| parent)
        };
        let mut ranked: BTreeMap<&str, (usize, &RemoteInstruction)> = BTreeMap::new();
        let mut local: BTreeMap<&str, &RemoteInstruction> = BTreeMap::new();
        for instruction in self
            .instructions
            .iter()
            .filter(|instruction| instruction_applies(&instruction.source.path, target_dir))
        {
            let path = instruction.source.path.as_str();
            let directory = instruction_directory(path);
            // A directory's personal overlay layers on top of whichever ranked
            // file it chose, rather than competing for the single slot.
            match ranked_instruction(path) {
                Some((rank, _)) => match ranked.get(directory) {
                    Some((selected, _)) if *selected <= rank => {}
                    _ => {
                        ranked.insert(directory, (rank, instruction));
                    }
                },
                None => {
                    local.insert(directory, instruction);
                }
            }
        }

        let mut directories: Vec<_> = ranked.keys().chain(local.keys()).copied().collect();
        directories.sort_unstable();
        directories.dedup();
        directories.sort_by_key(|directory| directory_depth(directory));
        directories
            .into_iter()
            .flat_map(|directory| {
                ranked
                    .get(directory)
                    .map(|(_, instruction)| *instruction)
                    .into_iter()
                    .chain(local.get(directory).copied())
            })
            .collect()
    }

    pub fn permission_digest(&self) -> Option<&str> {
        self.permissions.as_ref().map(|asset| asset.digest.as_str())
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ContextKey {
    authority: AuthorityIdentity,
    principal: AuthenticatedPrincipalId,
    project: ProjectIdentity,
    cwd: caudra_workspace::CwdHandle,
    revision: CollectionRevision,
}

#[derive(Default)]
pub struct RemoteProjectContextLoader {
    gate: async_lock::Mutex<()>,
    cache: Mutex<HashMap<ContextKey, Arc<RemoteProjectContext>>>,
}

impl RemoteProjectContextLoader {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn load(
        &self,
        session: &WorkspaceSession,
    ) -> Result<Arc<RemoteProjectContext>, RemoteProjectContextError> {
        let _guard = self.gate.lock().await;
        let service = session
            .workspace()
            .services()
            .assets
            .as_ref()
            .ok_or(RemoteProjectContextError::Unavailable)?;
        let manifest = service
            .discover(session.binding(), session.cursor())
            .await
            .map_err(|_| RemoteProjectContextError::Unavailable)?;
        validate_manifest(&manifest)?;
        let key = ContextKey {
            authority: session.binding().authority().clone(),
            principal: session.binding().principal().clone(),
            project: session.binding().project().clone(),
            cwd: session.cursor().cwd_handle().clone(),
            revision: manifest.revision.clone(),
        };
        if let Some(cached) = self
            .cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .cloned()
        {
            return Ok(cached);
        }

        let mut builder = ContextBuilder::new(manifest.revision);
        let mut declared_bytes = 0u64;
        let mut admitted = Vec::new();
        for asset in manifest.assets {
            // Skipping an asset costs context; loading one whose declaration
            // Caudra cannot account for costs more. A permission policy is the
            // exception, because a session that quietly runs without the
            // project's restrictive rules is weaker than one that refuses.
            match validate_asset(&asset) {
                Ok(()) => {}
                Err(error) if asset.kind == ProjectAssetKind::Permissions => return Err(error),
                Err(_) => {
                    builder.skip(asset.path);
                    continue;
                }
            }
            declared_bytes = declared_bytes.saturating_add(asset.size_bytes);
            if declared_bytes > MAX_TOTAL_BYTES {
                return Err(RemoteProjectContextError::InvalidManifest);
            }
            admitted.push(asset);
        }

        let mut total_bytes = 0u64;
        for asset in admitted {
            let max_bytes = asset_limit(&asset)?;
            let content = service
                .read(session.binding(), session.cursor(), &asset, max_bytes)
                .await
                .map_err(|_| RemoteProjectContextError::StaleAsset(asset.path.to_string()))?;
            if content.asset != asset || content.truncated {
                return Err(RemoteProjectContextError::StaleAsset(
                    asset.path.to_string(),
                ));
            }
            if content.content.len() > max_bytes as usize {
                return Err(RemoteProjectContextError::AssetTooLarge(
                    asset.path.to_string(),
                ));
            }
            total_bytes = total_bytes.saturating_add(content.content.len() as u64);
            if total_bytes > MAX_TOTAL_BYTES {
                return Err(RemoteProjectContextError::AssetTooLarge(
                    asset.path.to_string(),
                ));
            }
            builder.push(session, asset, content.content)?;
        }
        let context = Arc::new(builder.finish());
        if let Some(warning) = context.skipped_warning() {
            tracing::warn!(
                skipped = context.skipped().len(),
                detail = %warning,
                "remote project assets skipped"
            );
        }
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        if cache.len() >= MAX_CONTEXTS {
            cache.clear();
        }
        cache.insert(key, Arc::clone(&context));
        Ok(context)
    }
}

pub async fn load_remote_project_context(
    session: &WorkspaceSession,
) -> Result<Arc<RemoteProjectContext>, RemoteProjectContextError> {
    REMOTE_CONTEXT_LOADER.load(session).await
}

struct ContextBuilder {
    revision: CollectionRevision,
    instructions: Vec<RemoteInstruction>,
    commands: BTreeMap<String, CustomCommand>,
    command_tier: Option<&'static str>,
    skills: BTreeMap<String, RemoteSkill>,
    skill_tier: Option<&'static str>,
    workflows: Vec<RemoteWorkflow>,
    permissions: Option<RemotePermissionAsset>,
    skipped: Vec<WorkspacePath>,
}

impl ContextBuilder {
    fn new(revision: CollectionRevision) -> Self {
        Self {
            revision,
            instructions: Vec::new(),
            commands: BTreeMap::new(),
            command_tier: None,
            skills: BTreeMap::new(),
            skill_tier: None,
            workflows: Vec::new(),
            permissions: None,
            skipped: Vec::new(),
        }
    }

    fn skip(&mut self, path: WorkspacePath) {
        self.skipped.push(path);
    }

    fn push(
        &mut self,
        session: &WorkspaceSession,
        asset: ProjectAsset,
        content: String,
    ) -> Result<(), RemoteProjectContextError> {
        let identity = RemoteAssetIdentity {
            authority: session.binding().authority().clone(),
            principal: session.binding().principal().clone(),
            project: session.binding().project().clone(),
            path: asset.path.clone(),
            resource_id: asset.resource_id,
            revision: asset.revision,
        };
        match asset.kind {
            ProjectAssetKind::Instructions => self.instructions.push(RemoteInstruction {
                source: identity,
                content,
            }),
            ProjectAssetKind::Command => {
                let tier = first_component(identity.path.as_str());
                if self
                    .command_tier
                    .is_none_or(|selected| tier_rank(tier) < tier_rank(selected))
                {
                    self.command_tier = Some(tier);
                    self.commands.clear();
                }
                if self.command_tier == Some(tier)
                    && let Some(command) = parse_remote_command(&content, identity)
                {
                    self.commands.insert(command.name.clone(), command);
                }
            }
            ProjectAssetKind::Skill => {
                let tier = first_component(identity.path.as_str());
                if self
                    .skill_tier
                    .is_none_or(|selected| tier_rank(tier) < tier_rank(selected))
                {
                    self.skill_tier = Some(tier);
                    self.skills.clear();
                }
                if self.skill_tier == Some(tier) {
                    let (fields, body) = parse_frontmatter(&content);
                    if body.is_empty() {
                        return Err(RemoteProjectContextError::InvalidContent(
                            identity.path.to_string(),
                        ));
                    }
                    let directory = identity.path.as_str().split('/').nth(2).unwrap_or_default();
                    let name = fields
                        .get("name")
                        .cloned()
                        .unwrap_or_else(|| directory.to_owned());
                    self.skills.insert(
                        name.clone(),
                        RemoteSkill {
                            source: identity,
                            name,
                            description: fields.get("description").cloned().unwrap_or_default(),
                            content: body,
                        },
                    );
                }
            }
            ProjectAssetKind::Workflow => {
                let meta = parse_meta(&content).map_err(|_| {
                    RemoteProjectContextError::InvalidContent(identity.path.to_string())
                })?;
                let expected = format!("{}.rhai", meta.name);
                if identity.path.file_name() != expected {
                    return Err(RemoteProjectContextError::InvalidContent(
                        identity.path.to_string(),
                    ));
                }
                let digest = caudra_storage::workflow_trust::workflow_source_digest(
                    content.as_bytes(),
                    WORKFLOW_LANGUAGE_VERSION,
                    WORKFLOW_ABI_VERSION,
                );
                self.workflows.push(RemoteWorkflow {
                    source: identity,
                    meta,
                    content,
                    digest,
                });
            }
            ProjectAssetKind::Permissions => {
                if self.permissions.is_some() {
                    return Err(RemoteProjectContextError::InvalidManifest);
                }
                let declarations = parse_remote_permissions(content.as_bytes())?;
                self.permissions = Some(RemotePermissionAsset {
                    source: identity,
                    digest: sha256(content.as_bytes()),
                    declarations,
                });
            }
        }
        Ok(())
    }

    fn finish(mut self) -> RemoteProjectContext {
        self.instructions
            .sort_by(|left, right| left.source.path.cmp(&right.source.path));
        self.workflows
            .sort_by(|left, right| left.source.path.cmp(&right.source.path));
        self.skipped.sort();
        RemoteProjectContext {
            manifest_revision: self.revision,
            instructions: self.instructions,
            commands: self.commands.into_values().collect(),
            skills: self.skills.into_values().collect(),
            workflows: self.workflows,
            permissions: self.permissions,
            skipped: self.skipped,
        }
    }
}

fn validate_manifest(
    manifest: &caudra_workspace::ProjectAssetManifest,
) -> Result<(), RemoteProjectContextError> {
    if manifest.version.as_str() != MANIFEST_VERSION || manifest.assets.len() > MAX_ASSETS {
        return Err(RemoteProjectContextError::InvalidManifest);
    }
    let mut paths = HashSet::new();
    let mut resources = HashSet::new();
    for asset in &manifest.assets {
        if !paths.insert(asset.path.clone()) || !resources.insert(asset.resource_id.clone()) {
            return Err(RemoteProjectContextError::InvalidManifest);
        }
    }
    Ok(())
}

fn validate_asset(asset: &ProjectAsset) -> Result<(), RemoteProjectContextError> {
    let path = asset.path.as_str();
    let expected_trust = match asset.kind {
        ProjectAssetKind::Instructions if is_instruction_path(path) => {
            ProjectAssetTrust::Declarative
        }
        ProjectAssetKind::Skill if is_skill_path(path) => ProjectAssetTrust::Declarative,
        ProjectAssetKind::Command if is_command_path(path) => ProjectAssetTrust::Declarative,
        ProjectAssetKind::Workflow if is_workflow_path(path) => {
            ProjectAssetTrust::ClientApprovalRequired
        }
        ProjectAssetKind::Permissions if path == ".caudra/permissions.toml" => {
            ProjectAssetTrust::MixedReviewRequired
        }
        _ => return Err(RemoteProjectContextError::InvalidAsset(path.to_owned())),
    };
    if asset.trust != expected_trust || asset.size_bytes > u64::from(asset_limit(asset)?) {
        return Err(RemoteProjectContextError::InvalidAsset(path.to_owned()));
    }
    Ok(())
}

fn asset_limit(asset: &ProjectAsset) -> Result<u32, RemoteProjectContextError> {
    match asset.kind {
        ProjectAssetKind::Instructions => Ok(MAX_INSTRUCTION_BYTES),
        ProjectAssetKind::Command => Ok(MAX_COMMAND_BYTES),
        ProjectAssetKind::Skill => Ok(MAX_SKILL_BYTES),
        ProjectAssetKind::Workflow => {
            u32::try_from(MAX_SOURCE_BYTES).map_err(|_| RemoteProjectContextError::InvalidManifest)
        }
        ProjectAssetKind::Permissions => Ok(MAX_PERMISSION_BYTES),
    }
}

fn is_instruction_path(path: &str) -> bool {
    ranked_instruction(path).is_some() || ends_with_component(path, LOCAL_INSTRUCTION_FILE)
}

/// A candidate names a file (`AGENTS.md`) or a path (`.github/copilot-instructions.md`).
/// Either way it must sit on a directory boundary, so `src/copilot-instructions.md`
/// is an ordinary file while `src/.github/copilot-instructions.md` is not.
fn ends_with_component(path: &str, candidate: &str) -> bool {
    path == candidate
        || path
            .strip_suffix(candidate)
            .is_some_and(|parent| parent.ends_with('/'))
}

/// The rank a path competes at, and the candidate it matched, which is what
/// tells `.github/copilot-instructions.md` apart from the directory holding it.
fn ranked_instruction(path: &str) -> Option<(usize, &'static str)> {
    INSTRUCTION_FILES
        .iter()
        .position(|candidate| ends_with_component(path, candidate))
        .map(|rank| (rank, INSTRUCTION_FILES[rank]))
        .or_else(|| {
            ends_with_component(path, CAUDRA_INSTRUCTIONS)
                .then_some((INSTRUCTION_FILES.len(), CAUDRA_INSTRUCTIONS))
        })
}

fn is_skill_path(path: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    parts.len() == 4
        && SKILL_ROOTS.contains(&parts[0])
        && parts[1] == "skills"
        && !parts[2].is_empty()
        && parts[3] == "SKILL.md"
}

fn is_command_path(path: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    parts.len() == 3
        && COMMAND_ROOTS.contains(&parts[0])
        && parts[1] == "commands"
        && parts[2]
            .strip_suffix(".md")
            .is_some_and(|name| !name.is_empty())
}

fn is_workflow_path(path: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    parts.len() == 3
        && parts[..2] == [".caudra", "workflows"]
        && !parts[2].starts_with('.')
        && parts[2].ends_with(".rhai")
}

fn first_component(path: &str) -> &'static str {
    match path.split('/').next().unwrap_or_default() {
        ".caudra" => ".caudra",
        ".claude" => ".claude",
        ".opencode" => ".opencode",
        ".agents" => ".agents",
        _ => "",
    }
}

fn tier_rank(tier: &str) -> usize {
    match tier {
        ".caudra" => 0,
        ".claude" => 1,
        ".opencode" => 2,
        ".agents" => 3,
        _ => usize::MAX,
    }
}

fn instruction_applies(instruction: &WorkspacePath, target_dir: &str) -> bool {
    let directory = instruction_directory(instruction.as_str());
    directory == "." || target_dir == directory || target_dir.starts_with(&format!("{directory}/"))
}

/// The directory an instruction governs, which is the one holding it unless the
/// candidate is path-shaped: `.github/copilot-instructions.md` governs the
/// project root, not `.github`.
fn instruction_directory(path: &str) -> &str {
    let candidate = ranked_instruction(path).map_or(LOCAL_INSTRUCTION_FILE, |(_, name)| name);
    match path
        .strip_suffix(candidate)
        .and_then(|parent| parent.strip_suffix('/'))
    {
        Some(parent) if !parent.is_empty() => parent,
        _ => ".",
    }
}

fn directory_depth(directory: &str) -> usize {
    if directory == "." {
        0
    } else {
        directory.matches('/').count() + 1
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(SHA256_HEX_LEN);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScopeSet {
    All(bool),
    Scopes(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolDeclaration {
    allow: Option<ScopeSet>,
    ask: Option<ScopeSet>,
    deny: Option<ScopeSet>,
    default: Option<DefaultEffect>,
}

pub fn parse_remote_permissions(
    bytes: &[u8],
) -> Result<RemotePermissionDeclarations, RemoteProjectContextError> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        RemoteProjectContextError::InvalidContent(".caudra/permissions.toml".into())
    })?;
    let table: toml::Table = toml::from_str(text).map_err(|_| {
        RemoteProjectContextError::InvalidContent(".caudra/permissions.toml".into())
    })?;
    let mut declarations = RemotePermissionDeclarations::default();
    for (name, value) in table {
        if name == "default" {
            declarations.default = Some(value.try_into().map_err(|_| {
                RemoteProjectContextError::InvalidContent(".caudra/permissions.toml".into())
            })?);
            continue;
        }
        if name == "mcp" {
            parse_remote_mcp(value, &mut declarations)?;
            continue;
        }
        let key = ToolKey::parse(&name).map_err(|_| {
            RemoteProjectContextError::InvalidContent(".caudra/permissions.toml".into())
        })?;
        parse_tool_declaration(key, value, &mut declarations)?;
    }
    Ok(declarations)
}

fn parse_remote_mcp(
    value: toml::Value,
    declarations: &mut RemotePermissionDeclarations,
) -> Result<(), RemoteProjectContextError> {
    let servers = value.as_table().ok_or_else(permission_content_error)?;
    for (server, value) in servers {
        let table = value.as_table().ok_or_else(permission_content_error)?;
        for (tool, value) in table {
            let name = if tool == "default" {
                format!("{server}.*")
            } else {
                format!("{server}.{tool}")
            };
            let key = ToolKey::parse(&name).map_err(|_| permission_content_error())?;
            if tool == "default" {
                let effect: DefaultEffect = value
                    .clone()
                    .try_into()
                    .map_err(|_| permission_content_error())?;
                insert_default(key, effect, declarations);
            } else {
                parse_tool_declaration(key, value.clone(), declarations)?;
            }
        }
    }
    Ok(())
}

fn parse_tool_declaration(
    key: ToolKey,
    value: toml::Value,
    declarations: &mut RemotePermissionDeclarations,
) -> Result<(), RemoteProjectContextError> {
    let declaration: ToolDeclaration = value.try_into().map_err(|_| permission_content_error())?;
    append_rules(
        &key,
        declaration.deny,
        Effect::Deny,
        &mut declarations.restrictive_rules,
    )?;
    append_rules(
        &key,
        declaration.ask,
        Effect::Ask,
        &mut declarations.restrictive_rules,
    )?;
    append_rules(
        &key,
        declaration.allow,
        Effect::Allow,
        &mut declarations.allow_rules,
    )?;
    if let Some(effect) = declaration.default {
        insert_default(key, effect, declarations);
    }
    Ok(())
}

fn append_rules(
    key: &ToolKey,
    scopes: Option<ScopeSet>,
    effect: Effect,
    rules: &mut Vec<PermissionRule>,
) -> Result<(), RemoteProjectContextError> {
    let scopes = match scopes {
        None | Some(ScopeSet::All(false)) => return Ok(()),
        Some(ScopeSet::All(true)) => vec![None],
        Some(ScopeSet::Scopes(scopes)) => scopes.into_iter().map(Some).collect(),
    };
    for scope in scopes {
        if scope
            .as_ref()
            .is_some_and(|scope| scope.is_empty() || scope.len() > 4096)
        {
            return Err(permission_content_error());
        }
        rules.push(PermissionRule {
            tool: key.clone(),
            scope,
            effect,
        });
    }
    Ok(())
}

fn insert_default(
    key: ToolKey,
    effect: DefaultEffect,
    declarations: &mut RemotePermissionDeclarations,
) {
    match effect {
        DefaultEffect::Allow => {
            declarations.allow_defaults.insert(key, effect);
        }
        DefaultEffect::Deny | DefaultEffect::Prompt => {
            declarations.restrictive_defaults.insert(key, effect);
        }
    }
}

fn permission_content_error() -> RemoteProjectContextError {
    RemoteProjectContextError::InvalidContent(".caudra/permissions.toml".into())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use caudra_workspace::{
        CwdHandle, OperationId, ProjectAssetContent, ProjectAssetManifest, ProjectKey,
        ResourceScope, SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
        WorkspaceAssetService, WorkspaceCapabilities, WorkspaceCapability, WorkspaceCursor,
        WorkspaceError, WorkspaceHandle, WorkspaceServices,
    };

    use super::*;

    const ROOT_RULE: &str = "root rule";
    const NESTED_RULE: &str = "nested rule";
    const SHADOWED_RULE: &str = "shadowed rule";
    const MANIFEST_REVISION: &str = "manifest-1";
    const UNKNOWN_INSTRUCTION: &str = "docs/NOTES.md";
    const PERSONAL_RULE: &str = "personal rule";
    /// Pinned copy of `INSTRUCTION_FILES` in
    /// `workcell-mcp/crates/mcp-files/src/workspace.rs`. Update it deliberately,
    /// after teaching Caudra to accept whatever Workcell started declaring.
    const WORKCELL_INSTRUCTION_FILES: &[&str] = &[
        "AGENTS.md",
        "AGENTS.local.md",
        "CLAUDE.md",
        "COPILOT.md",
        ".cursorrules",
        ".windsurfrules",
        ".clinerules",
        "CONVENTIONS.md",
        "GEMINI.md",
        "CODING_AGENT.md",
    ];

    struct AssetState {
        manifest: ProjectAssetManifest,
        content: HashMap<WorkspacePath, String>,
    }

    pub(crate) struct AssetService {
        state: Mutex<AssetState>,
        discovers: AtomicUsize,
        reads: AtomicUsize,
        requested: Mutex<Vec<String>>,
        returned_asset: Mutex<Option<ProjectAsset>>,
    }

    impl AssetService {
        pub(crate) fn permission_fixture() -> (WorkspaceSession, Arc<Self>) {
            let content = "default = 'deny'\n";
            let service = Self::new(vec![(
                asset(
                    ".caudra/permissions.toml",
                    "deny-revision",
                    ProjectAssetKind::Permissions,
                    ProjectAssetTrust::MixedReviewRequired,
                    content.len() as u64,
                ),
                content,
            )]);
            (session(service.clone(), "permission-runtime"), service)
        }

        pub(crate) fn remove_permissions(&self) {
            let mut state = self.state.lock().unwrap();
            state.manifest.revision = CollectionRevision::new("removed").unwrap();
            state.manifest.assets.clear();
            state.content.clear();
        }

        pub(crate) fn corrupt_permissions(&self) {
            let content = "not valid permissions toml";
            self.replace(
                asset(
                    ".caudra/permissions.toml",
                    "broken",
                    ProjectAssetKind::Permissions,
                    ProjectAssetTrust::MixedReviewRequired,
                    content.len() as u64,
                ),
                content,
                "broken",
            );
        }

        fn new(assets: Vec<(ProjectAsset, &str)>) -> Arc<Self> {
            let content = assets
                .iter()
                .map(|(asset, content)| (asset.path.clone(), (*content).to_owned()))
                .collect();
            Arc::new(Self {
                state: Mutex::new(AssetState {
                    manifest: ProjectAssetManifest {
                        version: OperationId::new(MANIFEST_VERSION).unwrap(),
                        revision: CollectionRevision::new(MANIFEST_REVISION).unwrap(),
                        assets: assets.into_iter().map(|(asset, _)| asset).collect(),
                    },
                    content,
                }),
                discovers: AtomicUsize::new(0),
                reads: AtomicUsize::new(0),
                requested: Mutex::new(Vec::new()),
                returned_asset: Mutex::new(None),
            })
        }

        fn replace(&self, asset: ProjectAsset, content: &str, revision: &str) {
            let mut state = self.state.lock().unwrap();
            state.manifest.revision = CollectionRevision::new(revision).unwrap();
            state.manifest.assets = vec![asset.clone()];
            state.content = HashMap::from([(asset.path, content.to_owned())]);
        }

        fn return_asset(&self, asset: ProjectAsset) {
            *self.returned_asset.lock().unwrap() = Some(asset);
        }

        fn set_content(&self, path: &WorkspacePath, content: String) {
            self.state
                .lock()
                .unwrap()
                .content
                .insert(path.clone(), content);
        }
    }

    #[async_trait]
    impl WorkspaceAssetService for AssetService {
        async fn discover(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
        ) -> Result<ProjectAssetManifest, WorkspaceError> {
            self.discovers.fetch_add(1, Ordering::Relaxed);
            Ok(self.state.lock().unwrap().manifest.clone())
        }

        async fn read(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            asset: &ProjectAsset,
            max_bytes: u32,
        ) -> Result<ProjectAssetContent, WorkspaceError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.requested.lock().unwrap().push(asset.path.to_string());
            let state = self.state.lock().unwrap();
            let content = state.content.get(&asset.path).cloned().unwrap_or_default();
            Ok(ProjectAssetContent {
                asset: self
                    .returned_asset
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| asset.clone()),
                truncated: content.len() > max_bytes as usize,
                content,
            })
        }
    }

    fn asset(
        path: &str,
        revision: &str,
        kind: ProjectAssetKind,
        trust: ProjectAssetTrust,
        size_bytes: u64,
    ) -> ProjectAsset {
        ProjectAsset {
            path: WorkspacePath::new(path).unwrap(),
            resource_id: ResourceId::new(format!("resource-{}", path.replace('/', "-"))).unwrap(),
            revision: ResourceRevision::new(revision).unwrap(),
            kind,
            trust,
            size_bytes,
        }
    }

    fn session(service: Arc<AssetService>, subject: &str) -> WorkspaceSession {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-source").unwrap(),
            "test-authority",
            "test-workspace",
            "test-generation",
            "test-namespace",
        )
        .unwrap();
        let principal = AuthenticatedPrincipalId::new(authority.clone(), subject).unwrap();
        let project =
            ProjectIdentity::new(authority.clone(), ProjectKey::new("test-project").unwrap());
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new(format!("binding-{subject}")).unwrap(),
            authority.clone(),
            principal,
            project,
        )
        .unwrap();
        let root = ResourceId::new("root-resource").unwrap();
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(root),
            1,
            CwdHandle::new("cwd-handle").unwrap(),
        );
        let services = WorkspaceServices {
            assets: Some(service),
            ..WorkspaceServices::default()
        };
        let workspace = WorkspaceHandle::new(
            authority,
            WorkspaceCapabilities::from([
                WorkspaceCapability::ProjectAssetsDiscover,
                WorkspaceCapability::ProjectAssetsRead,
            ]),
            services,
        )
        .unwrap();
        WorkspaceSession::new(workspace, binding, cursor).unwrap()
    }

    #[test]
    fn forbidden_canaries_are_never_requested() {
        for path in [
            ".env",
            "init.lua",
            ".caudra/mcp.toml",
            ".caudra/plugins.toml",
        ] {
            let service = AssetService::new(vec![(
                asset(
                    path,
                    "revision-1",
                    ProjectAssetKind::Instructions,
                    ProjectAssetTrust::Declarative,
                    1,
                ),
                "x",
            )]);
            let context = smol::block_on(
                RemoteProjectContextLoader::new().load(&session(Arc::clone(&service), "alice")),
            )
            .unwrap();
            assert_eq!(context.skipped(), [WorkspacePath::new(path).unwrap()]);
            assert!(context.instructions().is_empty());
            assert_eq!(service.reads.load(Ordering::Relaxed), 0, "requested {path}");
        }
    }

    /// One file a project happens to keep must never cost the session every
    /// instruction, skill, command, and permission rule the project declares.
    #[test]
    fn an_unrecognised_asset_is_skipped_and_named_while_the_rest_load() {
        let service = AssetService::new(vec![
            (
                asset(
                    "AGENTS.md",
                    "asset-1",
                    ProjectAssetKind::Instructions,
                    ProjectAssetTrust::Declarative,
                    ROOT_RULE.len() as u64,
                ),
                ROOT_RULE,
            ),
            (
                asset(
                    UNKNOWN_INSTRUCTION,
                    "asset-2",
                    ProjectAssetKind::Instructions,
                    ProjectAssetTrust::Declarative,
                    NESTED_RULE.len() as u64,
                ),
                NESTED_RULE,
            ),
        ]);
        let context = smol::block_on(
            RemoteProjectContextLoader::new().load(&session(Arc::clone(&service), "alice")),
        )
        .unwrap();

        assert_eq!(context.instructions().len(), 1);
        assert_eq!(context.instructions()[0].content, ROOT_RULE);
        assert_eq!(
            context.skipped(),
            [WorkspacePath::new(UNKNOWN_INSTRUCTION).unwrap()]
        );
        let warning = context.skipped_warning().unwrap();
        assert!(warning.starts_with(SKIPPED_ASSETS_WARNING), "{warning}");
        assert!(warning.contains(UNKNOWN_INSTRUCTION), "{warning}");
        assert_eq!(service.reads.load(Ordering::Relaxed), 1);
    }

    /// Skipping costs context. Skipping a permission policy costs the project's
    /// restrictions, so that one refuses instead.
    #[test]
    fn a_permission_asset_that_fails_validation_still_refuses_the_context() {
        let service = AssetService::new(vec![(
            asset(
                ".caudra/permissions.toml",
                "asset-1",
                ProjectAssetKind::Permissions,
                ProjectAssetTrust::Declarative,
                1,
            ),
            "x",
        )]);

        let error = smol::block_on(
            RemoteProjectContextLoader::new().load(&session(Arc::clone(&service), "alice")),
        )
        .unwrap_err();

        assert!(matches!(error, RemoteProjectContextError::InvalidAsset(_)));
        assert_eq!(service.reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn content_is_cached_by_manifest_revision_and_refreshed_on_change() {
        let first = asset(
            "AGENTS.md",
            "asset-1",
            ProjectAssetKind::Instructions,
            ProjectAssetTrust::Declarative,
            ROOT_RULE.len() as u64,
        );
        let service = AssetService::new(vec![(first, ROOT_RULE)]);
        let session = session(Arc::clone(&service), "alice");
        let loader = RemoteProjectContextLoader::new();

        let initial = smol::block_on(loader.load(&session)).unwrap();
        let cached = smol::block_on(loader.load(&session)).unwrap();
        assert!(Arc::ptr_eq(&initial, &cached));
        assert_eq!(service.reads.load(Ordering::Relaxed), 1);

        let second = asset(
            "AGENTS.md",
            "asset-2",
            ProjectAssetKind::Instructions,
            ProjectAssetTrust::Declarative,
            NESTED_RULE.len() as u64,
        );
        service.replace(second, NESTED_RULE, "manifest-2");
        let refreshed = smol::block_on(loader.load(&session)).unwrap();
        assert_eq!(refreshed.instructions()[0].content, NESTED_RULE);
        assert_eq!(service.reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn cache_entries_are_isolated_by_authenticated_principal() {
        let instruction = asset(
            "AGENTS.md",
            "asset-1",
            ProjectAssetKind::Instructions,
            ProjectAssetTrust::Declarative,
            ROOT_RULE.len() as u64,
        );
        let service = AssetService::new(vec![(instruction, ROOT_RULE)]);
        let loader = RemoteProjectContextLoader::new();

        smol::block_on(loader.load(&session(Arc::clone(&service), "alice"))).unwrap();
        smol::block_on(loader.load(&session(Arc::clone(&service), "bob"))).unwrap();

        assert_eq!(service.reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn oversized_assets_are_skipped_before_any_content_is_requested() {
        let service = AssetService::new(vec![(
            asset(
                "AGENTS.md",
                "asset-1",
                ProjectAssetKind::Instructions,
                ProjectAssetTrust::Declarative,
                u64::from(MAX_INSTRUCTION_BYTES) + 1,
            ),
            ROOT_RULE,
        )]);
        let context = smol::block_on(
            RemoteProjectContextLoader::new().load(&session(Arc::clone(&service), "alice")),
        )
        .unwrap();
        assert!(context.instructions().is_empty());
        assert_eq!(
            context.skipped(),
            [WorkspacePath::new("AGENTS.md").unwrap()]
        );
        assert_eq!(service.reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn actual_content_cannot_exceed_the_total_manifest_budget() {
        let assets: Vec<_> = (0..9)
            .map(|index| {
                let path = format!(".caudra/skills/skill-{index}/SKILL.md");
                (
                    asset(
                        &path,
                        &format!("asset-{index}"),
                        ProjectAssetKind::Skill,
                        ProjectAssetTrust::Declarative,
                        1,
                    ),
                    "x",
                )
            })
            .collect();
        let service = AssetService::new(assets);
        let content = "x".repeat(MAX_SKILL_BYTES as usize);
        for index in 0..9 {
            service.set_content(
                &WorkspacePath::new(format!(".caudra/skills/skill-{index}/SKILL.md")).unwrap(),
                content.clone(),
            );
        }

        let error = smol::block_on(
            RemoteProjectContextLoader::new().load(&session(Arc::clone(&service), "alice")),
        )
        .unwrap_err();

        assert!(matches!(error, RemoteProjectContextError::AssetTooLarge(_)));
        assert_eq!(service.reads.load(Ordering::Relaxed), 9);
    }

    #[test]
    fn a_read_response_must_match_the_exact_manifest_asset() {
        let declared = asset(
            "AGENTS.md",
            "asset-1",
            ProjectAssetKind::Instructions,
            ProjectAssetTrust::Declarative,
            ROOT_RULE.len() as u64,
        );
        let service = AssetService::new(vec![(declared, ROOT_RULE)]);
        service.return_asset(asset(
            "CLAUDE.md",
            "asset-1",
            ProjectAssetKind::Instructions,
            ProjectAssetTrust::Declarative,
            ROOT_RULE.len() as u64,
        ));

        let error = smol::block_on(
            RemoteProjectContextLoader::new().load(&session(Arc::clone(&service), "alice")),
        )
        .unwrap_err();

        assert!(matches!(error, RemoteProjectContextError::StaleAsset(_)));
        assert_eq!(service.requested.lock().unwrap().as_slice(), ["AGENTS.md"]);
    }

    #[test]
    fn instruction_source_labels_do_not_expose_workspace_identity() {
        let instruction = asset(
            "AGENTS.md",
            "asset-1",
            ProjectAssetKind::Instructions,
            ProjectAssetTrust::Declarative,
            ROOT_RULE.len() as u64,
        );
        let context = smol::block_on(RemoteProjectContextLoader::new().load(&session(
            AssetService::new(vec![(instruction, ROOT_RULE)]),
            "alice",
        )))
        .unwrap();
        let label = context.instructions()[0].source.source_label();

        for private_identity in ["test-source", "test-authority", "alice", "test-project"] {
            assert!(!label.contains(private_identity));
        }
        assert!(label.contains("AGENTS.md"));
        assert!(label.contains("resource-AGENTS.md"));
        assert!(label.contains("asset-1"));
    }

    #[test]
    fn nested_instructions_apply_only_below_their_directory() {
        let service = AssetService::new(vec![
            (
                asset(
                    "AGENTS.md",
                    "root-revision",
                    ProjectAssetKind::Instructions,
                    ProjectAssetTrust::Declarative,
                    ROOT_RULE.len() as u64,
                ),
                ROOT_RULE,
            ),
            (
                asset(
                    "src/AGENTS.md",
                    "nested-revision",
                    ProjectAssetKind::Instructions,
                    ProjectAssetTrust::Declarative,
                    NESTED_RULE.len() as u64,
                ),
                NESTED_RULE,
            ),
            (
                asset(
                    "src/CLAUDE.md",
                    "shadowed-revision",
                    ProjectAssetKind::Instructions,
                    ProjectAssetTrust::Declarative,
                    SHADOWED_RULE.len() as u64,
                ),
                SHADOWED_RULE,
            ),
        ]);
        let context =
            smol::block_on(RemoteProjectContextLoader::new().load(&session(service, "alice")))
                .unwrap();

        let root = context.applicable_instructions(&WorkspacePath::new("README.md").unwrap());
        let nested = context.applicable_instructions(&WorkspacePath::new("src/lib.rs").unwrap());
        assert_eq!(root.len(), 1);
        assert_eq!(nested.len(), 2);
        assert_eq!(nested[0].content, ROOT_RULE);
        assert_eq!(nested[1].content, NESTED_RULE);
    }

    /// The overlay is personal, not a competitor: a directory that has both must
    /// contribute both, ranked file first.
    #[test]
    fn a_personal_overlay_layers_on_top_of_the_ranked_file_beside_it() {
        let instruction = |path: &str, revision: &str, body: &'static str| {
            (
                asset(
                    path,
                    revision,
                    ProjectAssetKind::Instructions,
                    ProjectAssetTrust::Declarative,
                    body.len() as u64,
                ),
                body,
            )
        };
        let service = AssetService::new(vec![
            instruction("AGENTS.md", "root", ROOT_RULE),
            instruction("AGENTS.local.md", "overlay", PERSONAL_RULE),
            instruction("src/AGENTS.local.md", "nested-overlay", NESTED_RULE),
        ]);
        let context =
            smol::block_on(RemoteProjectContextLoader::new().load(&session(service, "alice")))
                .unwrap();

        let root = context.applicable_instructions(&WorkspacePath::new("README.md").unwrap());
        assert_eq!(
            root.iter().map(|i| i.content.as_str()).collect::<Vec<_>>(),
            [ROOT_RULE, PERSONAL_RULE]
        );
        assert!(!root[0].is_personal_overlay());
        assert!(root[1].is_personal_overlay());

        let nested = context.applicable_instructions(&WorkspacePath::new("src/lib.rs").unwrap());
        assert_eq!(
            nested
                .iter()
                .map(|i| i.content.as_str())
                .collect::<Vec<_>>(),
            [ROOT_RULE, PERSONAL_RULE, NESTED_RULE]
        );
    }

    /// Precedence is shared with the local walk, so a project cannot be governed
    /// by one file on the host and a different one inside a sandbox.
    #[test]
    fn ranked_precedence_follows_the_shared_list() {
        for (better, worse) in INSTRUCTION_FILES
            .iter()
            .zip(INSTRUCTION_FILES.iter().skip(1))
        {
            let service = AssetService::new(vec![
                (
                    asset(
                        worse,
                        "worse",
                        ProjectAssetKind::Instructions,
                        ProjectAssetTrust::Declarative,
                        SHADOWED_RULE.len() as u64,
                    ),
                    SHADOWED_RULE,
                ),
                (
                    asset(
                        better,
                        "better",
                        ProjectAssetKind::Instructions,
                        ProjectAssetTrust::Declarative,
                        ROOT_RULE.len() as u64,
                    ),
                    ROOT_RULE,
                ),
            ]);
            let context =
                smol::block_on(RemoteProjectContextLoader::new().load(&session(service, "alice")))
                    .unwrap();

            let applicable = context.applicable_instructions(&WorkspacePath::root());
            assert_eq!(applicable.len(), 1, "{better} beside {worse}");
            assert_eq!(applicable[0].content, ROOT_RULE, "{better} beside {worse}");
        }
    }

    /// `.github/copilot-instructions.md` is matched by path, so a file that
    /// merely shares its basename stays an ordinary file.
    #[test]
    fn a_path_shaped_candidate_governs_the_root_and_matches_nothing_else() {
        assert!(is_instruction_path(".github/copilot-instructions.md"));
        assert_eq!(
            instruction_directory(".github/copilot-instructions.md"),
            "."
        );
        assert!(is_instruction_path("src/.github/copilot-instructions.md"));
        assert_eq!(
            instruction_directory("src/.github/copilot-instructions.md"),
            "src"
        );
        assert!(!is_instruction_path("src/copilot-instructions.md"));
        assert!(!is_instruction_path("docs/XAGENTS.md"));
    }

    /// Workcell walks the workspace and declares what it finds; Caudra decides
    /// what it will accept. The two lists are separate copies, so pin Workcell's
    /// here: this test is what makes the next addition visible instead of
    /// costing a session its whole project context.
    #[test]
    fn every_basename_workcell_declares_is_accepted() {
        for name in WORKCELL_INSTRUCTION_FILES {
            assert!(is_instruction_path(name), "{name}");
            assert!(is_instruction_path(&format!("src/{name}")), "{name}");
        }
    }

    #[test]
    fn command_and_skill_tiers_and_duplicates_are_deterministic() {
        let command = |path: &str, revision: &str, body: &'static str| {
            (
                asset(
                    path,
                    revision,
                    ProjectAssetKind::Command,
                    ProjectAssetTrust::Declarative,
                    body.len() as u64,
                ),
                body,
            )
        };
        let skill = |path: &str, revision: &str, body: &'static str| {
            (
                asset(
                    path,
                    revision,
                    ProjectAssetKind::Skill,
                    ProjectAssetTrust::Declarative,
                    body.len() as u64,
                ),
                body,
            )
        };
        let service = AssetService::new(vec![
            command(".opencode/commands/review.md", "c1", "old command"),
            command(".caudra/commands/review.md", "c2", "new command"),
            skill(
                ".agents/skills/review/SKILL.md",
                "s1",
                "---\nname: review\n---\nold skill",
            ),
            skill(
                ".caudra/skills/review/SKILL.md",
                "s2",
                "---\nname: review\n---\nnew skill",
            ),
        ]);
        let context =
            smol::block_on(RemoteProjectContextLoader::new().load(&session(service, "alice")))
                .unwrap();
        assert_eq!(context.commands().len(), 1);
        assert_eq!(context.commands()[0].content, "new command");
        assert_eq!(context.skills().len(), 1);
        assert_eq!(context.skills()[0].content, "new skill");
    }

    #[test]
    fn permission_denies_are_immediate_and_allows_are_separate() {
        let declarations =
            parse_remote_permissions(b"[shell]\ndeny = ['rm ']\nallow = ['git status']\n").unwrap();
        assert_eq!(declarations.restrictive_rules.len(), 1);
        assert_eq!(declarations.restrictive_rules[0].effect, Effect::Deny);
        assert_eq!(declarations.allow_rules.len(), 1);
        assert_eq!(declarations.allow_rules[0].effect, Effect::Allow);
    }

    #[test]
    fn permission_allows_require_exact_remote_asset_trust() {
        let source = "[shell]\ndeny = ['rm ']\nallow = ['git status']\n";
        let permission_asset = asset(
            ".caudra/permissions.toml",
            "permission-1",
            ProjectAssetKind::Permissions,
            ProjectAssetTrust::MixedReviewRequired,
            source.len() as u64,
        );
        let context = smol::block_on(RemoteProjectContextLoader::new().load(&session(
            AssetService::new(vec![(permission_asset, source)]),
            "alice",
        )))
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let state = caudra_storage::StateDir::from_path(temp.path().join("state"));
        let manager = crate::permissions::PermissionManager::new_persistent_in(
            PermissionsConfig::default(),
            temp.path().to_path_buf(),
            Arc::default(),
            state,
        );
        manager
            .replace_remote_permission_asset(context.permissions())
            .unwrap();
        let effects = || {
            manager
                .active_policy()
                .into_iter()
                .filter(|entry| entry.rule.tool == ToolKey::native("shell"))
                .map(|entry| entry.rule.effect)
                .collect::<Vec<_>>()
        };

        assert!(effects().contains(&Effect::Deny));
        assert!(!effects().contains(&Effect::Allow));
        assert!(manager.needs_project_permission_config_trust());

        manager.trust_project_permission_config().unwrap();
        assert!(effects().contains(&Effect::Allow));
        assert!(manager.project_permission_config_trusted());
    }

    #[test]
    fn workflow_trust_isolated_by_digest_revision_and_principal() {
        let source = "let meta = #{ name: \"review\", description: \"review\" };";
        let workflow_asset = asset(
            ".caudra/workflows/review.rhai",
            "workflow-1",
            ProjectAssetKind::Workflow,
            ProjectAssetTrust::ClientApprovalRequired,
            source.len() as u64,
        );
        let service = AssetService::new(vec![(workflow_asset, source)]);
        let alice = smol::block_on(
            RemoteProjectContextLoader::new().load(&session(Arc::clone(&service), "alice")),
        )
        .unwrap();
        let bob = smol::block_on(RemoteProjectContextLoader::new().load(&session(service, "bob")))
            .unwrap();
        let state = tempfile::tempdir().unwrap();
        let state = caudra_storage::StateDir::from_path(state.path().to_path_buf());
        let workflow = &alice.workflows()[0];
        let alice_key = workflow.source.trust_key().unwrap();
        caudra_storage::workflow_trust::trust_remote_workflow(&state, &alice_key, &workflow.digest)
            .unwrap();
        assert!(
            caudra_storage::workflow_trust::is_remote_workflow_trusted(
                &state,
                &alice_key,
                &workflow.digest,
            )
            .unwrap()
        );
        assert!(
            !caudra_storage::workflow_trust::is_remote_workflow_trusted(
                &state,
                &bob.workflows()[0].source.trust_key().unwrap(),
                &workflow.digest,
            )
            .unwrap()
        );
        assert!(
            !caudra_storage::workflow_trust::is_remote_workflow_trusted(
                &state,
                &alice_key,
                &format!("{}0", &workflow.digest[..workflow.digest.len() - 1]),
            )
            .unwrap()
        );
    }

    #[test]
    fn remote_mention_parsing_never_checks_a_local_path() {
        let found = crate::mentions::scan_remote("read @definitely-not-local.txt");
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].1.remote_path().map(WorkspacePath::as_str),
            Some("definitely-not-local.txt")
        );
        assert!(found[0].1.local_path().is_none());
    }
}
