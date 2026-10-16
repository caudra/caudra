//! MCP client: manages transports and routes tool calls to servers.
//!
//! Tool names are namespaced as `server.tool` so two servers can both expose `search`
//! without colliding. Names are deduped via `Arc<str>` in a small cache.
//!
//! All mutable state lives in the `run` task, which owns `McpManagerInner` exclusively.
//! Commands come in through a channel (one at a time, no interleaving). Reads go through
//! two lock-free `ArcSwap`s: a `ToolIndex` for tool calls and an `McpSnapshot` for the UI.
//! This way a slow tool call never blocks a toggle and vice versa.
//!
//! Servers connect inside `run`, not in `start`, so a slow `initialize` never delays the
//! caller's first frame. Whoever needs the tools waits on `McpHandle::ready`. Connects
//! land alongside commands, so one slow server holds up neither a fast one nor a shutdown.
//!
//! `McpSnapshotReader` is a read-only handle. Outside code physically cannot publish a
//! snapshot, so the "only `run` publishes" invariant is enforced by the type system.

pub mod call;
pub mod code;
pub mod config;
pub mod elicitation;
pub mod elicitor;
pub mod era;
pub mod error;
pub mod http;
pub mod inbound;
pub mod kind;
pub mod listen;
pub mod message;
pub mod oauth;
pub mod output;
pub mod protocol;
pub mod resources;
pub mod stdio;
pub mod transport;

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::tools::{ToolContext, ToolFilter};
use arc_swap::{ArcSwap, Guard};
use caudra_config::{ProfileToolExposure, ProfileToolPolicy, ProfileToolSource};
use caudra_storage::StateDir;
use caudra_storage::mcp_trust::{is_project_trusted, revoke_project_trust, trust_project};
use serde_json::{Value, json};
use smol::Timer;
use tracing::{info, warn};

use self::call::{CallScope, Elicitor};
use self::code::view::{ServerState, ServerView, ToolView, cut_instructions};
use self::config::{
    LocalExecutionPolicy, McpConfig, McpConfigErrors, McpConfigSource, McpReviewSummary,
    McpServerInfo, McpServerStatus, OauthClientConfig, RawServerConfig, RawTransport, ServerConfig,
    Transport, load_config, load_global_config, parse_server, requires_project_trust,
    resolve_http_addresses, review_summary, risky_ip, security_digest, transport_kind,
};
use self::elicitor::ElicitorFactory;
use self::error::McpError;
use self::http::HttpTransport;
use self::inbound::ListKind;
use self::kind::{McpCallFacts, McpServerKind, PausedRuns};
use self::output::McpOutput;
use self::protocol::{PromptInfo, ServerDiscovery, ToolAnnotations, ToolInfo};
use self::resources::{
    COMPLETIONS_FEATURE, CompletionRef, RESOURCES_FEATURE, ResourceContents, ResourceLists,
};
use self::stdio::StdioTransport;
use self::transport::McpTransport;
use crate::permissions::{PermissionSubject, canonical_json_sha256};
use crate::tools::profile_policy::{
    CEILING_DISABLED, MODE_DISABLED, PROFILE_DISABLED, PROFILE_LOADING,
};
use crate::tools::report::REASON_CONFIG;

pub(crate) const SEPARATOR: &str = ".";
pub(super) const WIRE_SEPARATOR: &str = "__";
pub const UNKNOWN_MCP: &str = "unknown_mcp";
const STARTUP_EPOCH: u64 = 0;
/// How long a server's list changes settle before it is listed again, so a
/// burst of them costs one listing.
pub const LIST_CHANGE_DEBOUNCE: Duration = Duration::from_millis(500);
const SCHEMA_REF_KEY: &str = "$ref";
const LOCAL_REF_PREFIX: char = '#';

/// Convert internal qualified name (`server.tool`) to wire format (`server__tool`)
/// for LLM provider APIs that reject dots in tool names.
///
/// Lossless: server names can't contain `__` (only alphanumeric + `-`),
/// so the first `__` in the wire name is always the separator boundary.
pub fn wire_tool_name(qualified: &str) -> String {
    qualified.replacen(SEPARATOR, WIRE_SEPARATOR, 1)
}

/// Convert wire format (`server__tool`) back to internal qualified name (`server.tool`).
///
/// Only the first `__` is the separator — tool names may contain underscores.
pub fn internal_tool_name(wire: &str) -> String {
    wire.replacen(WIRE_SEPARATOR, SEPARATOR, 1)
}
const MCP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

struct McpToolDef {
    qualified_name: Arc<str>,
    raw_name: String,
    description: String,
    input_schema: Value,
    meta: Arc<McpToolMeta>,
}

/// What a server said about a tool besides its name, description and input.
/// None of it reaches the model or the tool's permission contract.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpToolMeta {
    pub title: Option<String>,
    pub annotations: ToolAnnotations,
    pub output_schema: Option<Value>,
}

impl McpToolMeta {
    fn from_info(info: &mut ToolInfo) -> Self {
        Self {
            title: info.title.take(),
            annotations: std::mem::take(&mut info.annotations),
            output_schema: info.output_schema.take(),
        }
    }

    /// The tool's display title: its own, else the one its annotations give.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref().or(self.annotations.title.as_deref())
    }
}

/// The first `$ref` in `schema` that points outside it. Fetching one is
/// never safe, and a schema that needs it cannot be checked without it.
fn external_ref(schema: &Value) -> Option<&str> {
    match schema {
        Value::Object(fields) => fields.iter().find_map(|(key, value)| match value {
            Value::String(target)
                if key == SCHEMA_REF_KEY && !target.starts_with(LOCAL_REF_PREFIX) =>
            {
                Some(target.as_str())
            }
            _ => external_ref(value),
        }),
        Value::Array(items) => items.iter().find_map(external_ref),
        _ => None,
    }
}

/// The tools of `server` a model can be offered. A name that is invalid or
/// too long on the wire, or an input schema with an external `$ref`, drops
/// the tool with a warning.
fn tool_defs(server: &str, infos: Vec<ToolInfo>) -> Vec<McpToolDef> {
    infos
        .into_iter()
        .filter(|info| {
            if !config::is_valid_tool_name(&info.name) {
                warn!(tool = %info.name, server, "skipping tool with invalid name");
                return false;
            }
            // Wire format is server__tool — check total length fits LLM API limits
            let wire_len = server.len() + 2 + info.name.len();
            if wire_len > 64 {
                warn!(
                    tool = %info.name,
                    server,
                    wire_len,
                    "skipping tool — wire name exceeds 64 char LLM API limit"
                );
                return false;
            }
            if let Some(target) = external_ref(&info.input_schema) {
                warn!(tool = %info.name, server, target, "skipping tool whose input schema references an external $ref");
                return false;
            }
            true
        })
        .map(|mut info| McpToolDef {
            qualified_name: intern(format!("{server}{SEPARATOR}{}", info.name)),
            meta: Arc::new(McpToolMeta::from_info(&mut info)),
            raw_name: info.name,
            description: info.description,
            input_schema: info.input_schema,
        })
        .collect()
}

struct McpPromptDef {
    qualified_name: String,
    raw_name: String,
    description: String,
    arguments: Vec<protocol::PromptArgument>,
}

impl McpPromptDef {
    fn from_info(server_name: &str, info: protocol::PromptInfo) -> Self {
        Self {
            qualified_name: format!("{server_name}{SEPARATOR}{}", info.name),
            raw_name: info.name,
            description: info.description.unwrap_or_default(),
            arguments: info.arguments,
        }
    }

    fn to_info(&self, server_name: &str, completions: bool) -> McpPromptInfo {
        McpPromptInfo {
            completions,
            display_name: format!("{server_name}:{}", self.raw_name),
            qualified_name: self.qualified_name.clone(),
            description: self.description.clone(),
            arguments: self
                .arguments
                .iter()
                .map(|a| McpPromptArg {
                    name: a.name.clone(),
                    description: a.description.clone().unwrap_or_default(),
                    required: a.required,
                })
                .collect(),
        }
    }
}

struct ServerEntry {
    name: String,
    config: Option<ServerConfig>,
    transport_kind: &'static str,
    origin: PathBuf,
    source: McpConfigSource,
    review: McpReviewSummary,
    authority_digest: String,
    trust: Option<ProjectTrust>,
    trusted_once: bool,
    trust_rejected: bool,
    status: McpServerStatus,
    transport: Option<Arc<dyn McpTransport>>,
    /// What the running server said about itself when it connected.
    discovery: Option<ServerDiscovery>,
    /// Lists the running server again when it says a list changed. Dropped
    /// with the connection.
    list_watch: Option<smol::Task<()>>,
    /// Executor runs this connection reported paused. Replaced with it.
    paused_runs: Arc<PausedRuns>,
    tools: Vec<McpToolDef>,
    prompts: Vec<McpPromptDef>,
    resources: ResourceLists,
    /// `STARTUP_EPOCH`, or the generation of the reload that built this entry.
    /// A connect lands only on the entry it was started for, never on a successor.
    epoch: u64,
}

#[derive(Clone)]
struct ProjectTrust {
    project: Option<PathBuf>,
    config_digest: String,
}

impl ServerEntry {
    async fn clear_connection(&mut self) {
        if let Some(old) = self.transport.take() {
            // A live tool call can still be holding an `Arc` to this transport via the
            // `ToolIndex`, so we cannot rely on `Drop` to reap the child in time.
            kill_process_groups(&old.child_pids());
            old.shutdown().await;
        }
        self.list_watch = None;
        self.discovery = None;
        self.paused_runs = Arc::default();
        self.tools.clear();
        self.prompts.clear();
        self.resources = ResourceLists::default();
    }

    fn populate(&mut self, result: StartResult) {
        let StartResult {
            transport,
            discovery,
            tool_infos,
            prompt_infos,
            resources,
        } = result;
        self.tools = tool_defs(&self.name, tool_infos);
        self.prompts = prompt_defs(&self.name, prompt_infos);
        self.resources = resources;
        self.transport = Some(transport);
        self.discovery = Some(discovery);
        self.list_watch = None;
        self.paused_runs = Arc::default();
        self.status = McpServerStatus::Running;
    }

    fn kind(&self) -> Option<McpServerKind> {
        self.discovery.as_ref().and_then(ServerDiscovery::kind)
    }

    /// Lists the running server again, after [`LIST_CHANGE_DEBOUNCE`], each
    /// time it says its tools, prompts or resources changed.
    fn watch_lists(&mut self, relisted: &flume::Sender<Relisted>) {
        if self.list_watch.is_some() {
            return;
        }
        let Some(connection) = self.transport.clone() else {
            return;
        };
        let Some(changes) = connection.inbound().map(|inbound| inbound.list_changes()) else {
            return;
        };
        let server = self.name.clone();
        let relisted = relisted.clone();
        let offers_resources = self
            .discovery
            .as_ref()
            .is_some_and(|discovery| discovery.capabilities.resources.is_some());
        self.list_watch = Some(smol::spawn(async move {
            while let Ok(first) = changes.recv_async().await {
                Timer::after(LIST_CHANGE_DEBOUNCE).await;
                let changed: HashSet<ListKind> = changes.drain().chain([first]).collect();
                let tools = if changed.contains(&ListKind::Tools) {
                    Some(transport::list_tools(connection.as_ref()).await)
                } else {
                    None
                };
                let prompts = if changed.contains(&ListKind::Prompts) {
                    Some(transport::list_prompts(connection.as_ref()).await)
                } else {
                    None
                };
                let resources = if offers_resources && changed.contains(&ListKind::Resources) {
                    Some(resources::list_all(connection.as_ref()).await)
                } else {
                    None
                };
                if tools.is_none() && prompts.is_none() && resources.is_none() {
                    continue;
                }
                let update = Relisted {
                    server: server.clone(),
                    connection: Arc::clone(&connection),
                    tools,
                    prompts,
                    resources,
                };
                if relisted.send_async(update).await.is_err() {
                    return;
                }
            }
        }));
    }
}

fn prompt_defs(server: &str, infos: Vec<PromptInfo>) -> Vec<McpPromptDef> {
    infos
        .into_iter()
        .map(|info| McpPromptDef::from_info(server, info))
        .collect()
}

/// A running server's lists, fetched again after it said they changed.
/// `None` for a list it did not say changed.
struct Relisted {
    server: String,
    connection: Arc<dyn McpTransport>,
    tools: Option<Result<Vec<ToolInfo>, McpError>>,
    prompts: Option<Result<Vec<PromptInfo>, McpError>>,
    resources: Option<Result<ResourceLists, McpError>>,
}

/// Lands a relisting on the connection it was fetched over, and nowhere
/// else. A list that failed keeps the one before it. Whether anything
/// landed, and so needs publishing.
fn apply_relisted(inner: &mut McpManagerInner, relisted: Relisted) -> bool {
    let Some(entry) = inner.entries.iter_mut().find(|entry| {
        entry.name == relisted.server
            && entry
                .transport
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &relisted.connection))
    }) else {
        return false;
    };
    let mut landed = false;
    match relisted.tools {
        Some(Ok(tools)) => {
            entry.tools = tool_defs(&entry.name, tools);
            landed = true;
        }
        Some(Err(error)) => {
            warn!(server = %entry.name, %error, "MCP tools/list after a list change failed")
        }
        None => {}
    }
    match relisted.prompts {
        Some(Ok(prompts)) => {
            entry.prompts = prompt_defs(&entry.name, prompts);
            landed = true;
        }
        Some(Err(error)) => {
            warn!(server = %entry.name, %error, "MCP prompts/list after a list change failed")
        }
        None => {}
    }
    match relisted.resources {
        Some(Ok(resources)) => {
            entry.resources = resources;
            landed = true;
        }
        Some(Err(error)) => {
            warn!(server = %entry.name, %error, "MCP resources/list after a list change failed")
        }
        None => {}
    }
    if landed {
        info!(
            server = %entry.name,
            tools = entry.tools.len(),
            prompts = entry.prompts.len(),
            resources = entry.resources.resources.len(),
            templates = entry.resources.templates.len(),
            "MCP server lists changed"
        );
    }
    landed
}

struct McpManagerInner {
    entries: Vec<ServerEntry>,
    state_dir: Option<StateDir>,
    generation: u64,
    /// Fixed at startup: a reload may never relax how local servers run.
    local_execution: LocalExecutionPolicy,
}

#[derive(Default)]
struct ToolIndex {
    tools: HashMap<Arc<str>, ToolRef>,
    prompts: HashMap<String, PromptRef>,
    servers: HashMap<String, ServerRef>,
    descriptors: Arc<[ToolDescriptor]>,
}

/// A running server, for what it offers beyond tools and prompts.
struct ServerRef {
    transport: Arc<dyn McpTransport>,
    resources: bool,
    completions: bool,
}

impl ToolIndex {
    fn read_only(&self, qualified_name: &str) -> bool {
        self.tools
            .get(qualified_name)
            .is_some_and(ToolRef::read_only)
    }
}

/// One published MCP tool, as code mode presents it.
struct ToolDescriptor {
    qualified_name: Arc<str>,
    server: String,
    view: ToolView,
}

#[derive(Clone)]
struct ToolRef {
    qualified_name: Arc<str>,
    raw_name: String,
    subject: PermissionSubject,
    generation: u64,
    transport: Arc<dyn McpTransport>,
    meta: Arc<McpToolMeta>,
    kind: Option<McpServerKind>,
    paused_runs: Arc<PausedRuns>,
}

impl ToolRef {
    /// A recognised server annotated the tool `readOnlyHint`, so it may run
    /// where Caudra's own read tools do. A stranger's annotation earns nothing.
    fn read_only(&self) -> bool {
        self.kind.is_some() && self.meta.annotations.read_only()
    }
}

#[derive(Clone)]
pub struct McpToolBinding {
    tool: ToolRef,
}

impl McpToolBinding {
    pub fn qualified_name(&self) -> &str {
        &self.tool.qualified_name
    }

    pub fn subject(&self) -> &PermissionSubject {
        &self.tool.subject
    }

    pub fn generation(&self) -> u64 {
        self.tool.generation
    }

    /// The server this tool belongs to, when Caudra knows it by name.
    pub fn kind(&self) -> Option<McpServerKind> {
        self.tool.kind
    }

    pub fn annotations(&self) -> &ToolAnnotations {
        &self.tool.meta.annotations
    }

    pub fn title(&self) -> Option<&str> {
        self.tool.meta.title()
    }

    pub fn read_only(&self) -> bool {
        self.tool.read_only()
    }

    /// What a permission prompt for a call with `args` should know, for a
    /// tool of a recognised server.
    pub fn call_facts(&self, args: &Value) -> Option<McpCallFacts> {
        let kind = self.tool.kind?;
        Some(McpCallFacts {
            kind,
            read_only: self.read_only(),
            note: kind.always_asks(&self.tool.raw_name, args, &self.tool.paused_runs),
        })
    }

    /// Calls the tool. An Executor run the result says is paused is
    /// remembered, so a later `resume` prompt can say what it approves.
    pub async fn call(&self, args: &Value, scope: &CallScope) -> Result<McpOutput, McpError> {
        let output = transport::call_tool(
            self.tool.transport.as_ref(),
            &self.tool.raw_name,
            &self.tool.meta,
            self.tool.kind,
            args,
            scope,
        )
        .await?;
        if self.tool.kind == Some(McpServerKind::Executor)
            && let Some(structured) = output.structured_content()
        {
            self.tool.paused_runs.record(structured);
        }
        Ok(output)
    }

    /// What `output`, flagged `isError`, reads as to the model.
    pub fn reported_error(&self, output: &McpOutput) -> McpError {
        transport::reported_error(self.tool.transport.server_name(), output)
    }
}

struct PromptRef {
    raw_name: String,
    transport: Arc<dyn McpTransport>,
    completions: bool,
}

#[derive(Clone, Default)]
pub struct McpPromptInfo {
    pub display_name: String,
    pub qualified_name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArg>,
    /// Whether its server completes argument values.
    pub completions: bool,
}

#[derive(Clone)]
pub struct McpPromptArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

#[derive(Clone, Default)]
pub struct McpSnapshot {
    pub infos: Vec<McpServerInfo>,
    pub prompts: Vec<McpPromptInfo>,
    /// Every running server that declared resources, in server order.
    pub resources: Vec<McpServerResources>,
    pub pids: Vec<u32>,
    pub generation: u64,
}

/// A running server's resources and templates, and whether it completes
/// their variables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpServerResources {
    pub server: String,
    pub lists: ResourceLists,
    pub completions: bool,
}

impl McpSnapshot {
    pub fn resources_of(&self, server: &str) -> Option<&McpServerResources> {
        self.resources.iter().find(|offer| offer.server == server)
    }

    /// Whether `uri` is something the running `server` listed, or spells the
    /// way one of its templates names a resource.
    pub fn offers_resource(&self, server: &str, uri: &str) -> bool {
        self.resources_of(server)
            .is_some_and(|offer| offer.lists.offers(uri))
    }
}

/// What a completion is asked for: a prompt's argument, or a variable of one
/// of a server's resource templates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CompletionTarget {
    Prompt {
        qualified_name: String,
    },
    Template {
        server: String,
        uri_template: String,
    },
}

/// How one MCP tool reaches the model: listed in the `# MCP servers` reminder, found only
/// through `search()`, or not at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolStatus {
    pub qualified_name: String,
    pub server: String,
    pub disabled: bool,
    pub search_only: bool,
    pub reason: Option<&'static str>,
}

/// Read-only view of the latest published `McpSnapshot`. Handing this out instead of the
/// raw `ArcSwap` keeps outside code from publishing snapshots of its own.
#[derive(Clone)]
pub struct McpSnapshotReader {
    snapshot: Arc<ArcSwap<McpSnapshot>>,
    index: Arc<ArcSwap<ToolIndex>>,
}

impl McpSnapshotReader {
    pub fn empty() -> Self {
        Self::from_snapshot(McpSnapshot::default())
    }

    pub fn from_snapshot(snapshot: McpSnapshot) -> Self {
        Self {
            snapshot: Arc::new(ArcSwap::from_pointee(snapshot)),
            index: Arc::default(),
        }
    }

    pub fn load(&self) -> Guard<Arc<McpSnapshot>> {
        self.snapshot.load()
    }

    pub fn load_full(&self) -> Arc<McpSnapshot> {
        self.snapshot.load_full()
    }

    /// Asks the server behind `target` for values of `argument`, typed so far
    /// as `value`. A completion changes nothing, so a view may ask for one; a
    /// server that never declared `completions` is not asked at all.
    pub async fn complete(
        &self,
        target: &CompletionTarget,
        argument: &str,
        value: &str,
        context: &BTreeMap<String, String>,
    ) -> Result<resources::Completion, McpError> {
        let (transport, reference) = {
            let index = self.index.load();
            match target {
                CompletionTarget::Prompt { qualified_name } => {
                    let prompt = index.prompts.get(qualified_name).ok_or_else(|| {
                        McpError::UnknownPrompt {
                            name: qualified_name.clone(),
                        }
                    })?;
                    if !prompt.completions {
                        return Err(McpError::Unsupported {
                            server: prompt.transport.server_name().to_string(),
                            feature: COMPLETIONS_FEATURE,
                        });
                    }
                    (
                        Arc::clone(&prompt.transport),
                        CompletionRef::Prompt(prompt.raw_name.clone()),
                    )
                }
                CompletionTarget::Template {
                    server,
                    uri_template,
                } => {
                    let running =
                        index
                            .servers
                            .get(server)
                            .ok_or_else(|| McpError::ServerDied {
                                server: server.clone(),
                            })?;
                    if !running.completions {
                        return Err(McpError::Unsupported {
                            server: server.clone(),
                            feature: COMPLETIONS_FEATURE,
                        });
                    }
                    (
                        Arc::clone(&running.transport),
                        CompletionRef::Resource(uri_template.clone()),
                    )
                }
            }
        };
        resources::complete(transport.as_ref(), &reference, argument, value, context).await
    }
}

impl Default for McpSnapshotReader {
    fn default() -> Self {
        Self::empty()
    }
}

pub enum McpCommand {
    Toggle {
        server: String,
        enabled: bool,
    },
    Reconnect {
        server: String,
    },
    TrustOnce {
        server: String,
    },
    TrustProject {
        server: String,
    },
    Reject {
        server: String,
    },
    /// Diff-apply a freshly loaded config; see `McpHandle::reload`.
    Reload {
        config: McpConfig,
    },
    /// Drain every running transport and stop the loop. The loop sends `()` on `ack` once
    /// every shutdown has finished, so callers can wait with a timeout.
    Shutdown {
        ack: flume::Sender<()>,
    },
}

#[derive(Clone)]
pub struct McpHandle {
    cmd_tx: flume::Sender<McpCommand>,
    index: Arc<ArcSwap<ToolIndex>>,
    snapshot: Arc<ArcSwap<McpSnapshot>>,
    /// Nothing is ever sent on it. `run` drops the sender once the first
    /// connect pass has published, and that disconnect is the signal.
    ready_rx: flume::Receiver<Infallible>,
}

/// One agent's view of MCP: the shared handle, and the policy that decides which tools this agent
/// may reach.
#[derive(Clone)]
pub struct McpSession {
    handle: McpHandle,
    /// The MCP-qualified entries of config's `disabled_tools`. Held here rather
    /// than in `ToolFilter` because MCP tools never pass through the registry.
    disabled: Arc<[String]>,
    profile: Arc<ProfileToolPolicy>,
    ceiling: ToolFilter,
    inherited_profiles: Vec<Arc<ProfileToolPolicy>>,
    /// How the front end asks the user what a server wants to know; `None`
    /// declines every such request.
    elicitors: Option<Arc<dyn ElicitorFactory>>,
}

impl std::ops::Deref for McpSession {
    type Target = McpHandle;
    fn deref(&self) -> &McpHandle {
        &self.handle
    }
}

impl McpSession {
    pub fn new(handle: McpHandle) -> Self {
        Self {
            handle,
            disabled: Arc::from([]),
            profile: Arc::default(),
            ceiling: ToolFilter::All,
            inherited_profiles: Vec::new(),
            elicitors: None,
        }
    }

    pub fn with_elicitors(mut self, elicitors: Option<Arc<dyn ElicitorFactory>>) -> Self {
        self.elicitors = elicitors;
        self
    }

    /// Who answers the servers during a call made under `ctx`, if anyone can.
    pub fn elicitor_for(&self, ctx: &ToolContext) -> Option<Arc<dyn Elicitor>> {
        self.elicitors.as_ref()?.for_call(ctx)
    }

    /// Config's `disabled_tools`, keeping only what can name an MCP tool.
    /// Built-in names are already handled by `ToolFilter`.
    pub fn with_disabled_tools(mut self, disabled_tools: &[String]) -> Self {
        self.disabled = disabled_tools
            .iter()
            .filter(|name| name.contains(SEPARATOR))
            .cloned()
            .collect();
        self
    }

    pub fn is_disabled(&self, qualified_name: &str) -> bool {
        policy_disabled(
            &self.disabled,
            &self.profile,
            &self.ceiling,
            qualified_name,
            self.handle.is_read_only(qualified_name),
        ) || self.inherited_profiles.iter().any(|profile| {
            profile.exposure(qualified_name, ProfileToolSource::Mcp)
                == Some(ProfileToolExposure::Disabled)
        })
    }

    /// A tool the profile loads lazily is reachable through `search()` but never listed.
    pub fn is_search_only(&self, qualified_name: &str) -> bool {
        self.profile
            .exposure(qualified_name, ProfileToolSource::Mcp)
            == Some(ProfileToolExposure::Lazy)
    }

    pub fn with_profile_policy(
        mut self,
        profile: Arc<ProfileToolPolicy>,
        ceiling: ToolFilter,
    ) -> Self {
        self.profile = profile;
        self.ceiling = ceiling;
        self
    }

    pub fn with_actor_policy(mut self, profile: Arc<ProfileToolPolicy>) -> Self {
        self.profile = profile;
        self
    }

    pub fn narrowed(mut self, ceiling: &ToolFilter) -> Self {
        self.ceiling = self.ceiling.intersect(ceiling);
        self
    }

    pub fn freeze_actor_ceiling(mut self) -> Self {
        if !self.profile.is_legacy() {
            self.inherited_profiles.push(Arc::clone(&self.profile));
        }
        self
    }

    /// Every enabled server and the tools this agent may call on it, as code mode presents
    /// them, in configuration order.
    pub fn servers(&self) -> Vec<ServerView> {
        let index = self.handle.index.load();
        let snapshot = self.handle.snapshot.load();
        snapshot
            .infos
            .iter()
            .filter_map(|info| {
                let state = match &info.status {
                    McpServerStatus::Disabled => return None,
                    McpServerStatus::Running => ServerState::Connected,
                    McpServerStatus::Connecting => ServerState::Connecting,
                    McpServerStatus::AwaitingTrust => ServerState::AwaitingTrust,
                    McpServerStatus::NeedsAuth { .. } | McpServerStatus::Refused => {
                        ServerState::NeedsLogin
                    }
                    McpServerStatus::Failed(message) => ServerState::Failed(message.clone()),
                };
                let discovery = info.discovery.as_ref();
                Some(ServerView {
                    name: info.name.clone(),
                    title: discovery
                        .and_then(|discovery| discovery.server_info.title.clone())
                        .filter(|title| *title != info.name),
                    state,
                    instructions: discovery
                        .and_then(|discovery| discovery.instructions.as_deref())
                        .map(cut_instructions),
                    tools: index
                        .descriptors
                        .iter()
                        .filter(|descriptor| {
                            descriptor.server == info.name
                                && !self.is_disabled(&descriptor.qualified_name)
                        })
                        .map(|descriptor| ToolView {
                            search_only: self.is_search_only(&descriptor.qualified_name),
                            ..descriptor.view.clone()
                        })
                        .collect(),
                    resources: snapshot
                        .resources_of(&info.name)
                        .is_some_and(|offer| !offer.lists.is_empty()),
                })
            })
            .collect()
    }

    /// Every published MCP tool and how it reaches this agent.
    pub fn tool_inventory(&self) -> Vec<McpToolStatus> {
        let index = self.handle.index.load();
        index
            .descriptors
            .iter()
            .map(|descriptor| {
                let name = &descriptor.qualified_name;
                let disabled = self.is_disabled(name);
                let reason = if disabled {
                    Some(if is_disabled(&self.disabled, name) {
                        REASON_CONFIG
                    } else if self.ceiling.is_read_only() && !index.read_only(name) {
                        MODE_DISABLED
                    } else if !self.ceiling.matches(name) {
                        CEILING_DISABLED
                    } else {
                        PROFILE_DISABLED
                    })
                } else {
                    self.profile
                        .exposure(name, ProfileToolSource::Mcp)
                        .map(|_| PROFILE_LOADING)
                };
                McpToolStatus {
                    qualified_name: name.to_string(),
                    server: descriptor.server.clone(),
                    disabled,
                    search_only: !disabled && self.is_search_only(name),
                    reason,
                }
            })
            .collect()
    }
}

/// A read-only ceiling keeps only tools a recognised server annotated
/// read-only (`read_only`).
fn policy_disabled(
    patterns: &[String],
    profile: &ProfileToolPolicy,
    ceiling: &ToolFilter,
    name: &str,
    read_only: bool,
) -> bool {
    is_disabled(patterns, name)
        || (ceiling.is_read_only() && !read_only)
        || !ceiling.matches(name)
        || profile.exposure(name, ProfileToolSource::Mcp) == Some(ProfileToolExposure::Disabled)
}

fn is_disabled(patterns: &[String], qualified_name: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| caudra_config::tool_pattern_matches(pattern, qualified_name))
}

impl McpHandle {
    pub fn send(&self, cmd: McpCommand) {
        if let Err(e) = self.cmd_tx.try_send(cmd) {
            warn!(error = %e, "MCP command loop is gone");
        }
    }

    /// Resolves once every enabled server has connected or failed. Await it
    /// before building a request's tool list, or an early prompt ships without
    /// the MCP tools.
    pub async fn ready(&self) {
        let _ = self.ready_rx.recv_async().await;
    }

    /// Whether the first connect pass is still running, so the tools of the
    /// servers it has yet to reach are missing from the index.
    pub fn connecting(&self) -> bool {
        !self.ready_rx.is_disconnected()
    }

    pub fn reader(&self) -> McpSnapshotReader {
        McpSnapshotReader {
            snapshot: Arc::clone(&self.snapshot),
            index: Arc::clone(&self.index),
        }
    }

    /// Moves the running servers onto `config`, loaded by `config::try_load_config`,
    /// or `config::try_load_global_config` for a `start_global*` handle; a config
    /// loaded under another execution policy is ignored. A server whose definition and
    /// enabled state held keeps its connection; a removed or disabled one stops
    /// and its tools leave the index; an added or changed one is rebuilt under
    /// the startup rules, so one whose reviewed definition changed parks as
    /// `AwaitingTrust` rather than starting. Runtime servers stay unless a file
    /// now claims the name.
    pub fn reload(&self, config: McpConfig) {
        self.send(McpCommand::Reload { config });
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.index.load().tools.contains_key(name)
    }

    /// Whether configuration enables some server, which is what lists servers in the
    /// `# MCP servers` reminder. Whether it has connected does not matter.
    pub fn has_enabled_server(&self) -> bool {
        self.snapshot
            .load()
            .infos
            .iter()
            .any(|info| !matches!(info.status, McpServerStatus::Disabled))
    }

    /// Whether `name` is a recognised server's tool annotated read-only,
    /// which plan mode and read-only policies keep.
    pub fn is_read_only(&self, name: &str) -> bool {
        self.index.load().read_only(name)
    }

    pub fn interned_name(&self, name: &str) -> Arc<str> {
        self.index
            .load()
            .tools
            .get_key_value(name)
            .map(|(k, _)| Arc::clone(k))
            .unwrap_or_else(|| Arc::from(UNKNOWN_MCP))
    }

    pub fn bind_tool(&self, qualified_name: &str) -> Result<McpToolBinding, McpError> {
        let index = self.index.load();
        let tool =
            index
                .tools
                .get(qualified_name)
                .cloned()
                .ok_or_else(|| McpError::UnknownTool {
                    name: qualified_name.into(),
                })?;
        Ok(McpToolBinding { tool })
    }

    pub async fn call_tool(
        &self,
        qualified_name: &str,
        args: &Value,
        scope: &CallScope,
    ) -> Result<McpOutput, McpError> {
        self.bind_tool(qualified_name)?.call(args, scope).await
    }

    pub async fn get_prompt(
        &self,
        qualified_name: &str,
        arguments: &HashMap<String, String>,
        scope: &CallScope,
    ) -> Result<Vec<protocol::PromptMessage>, McpError> {
        let (raw_name, transport) = {
            let idx = self.index.load();
            let Some(p) = idx.prompts.get(qualified_name) else {
                return Err(McpError::UnknownPrompt {
                    name: qualified_name.into(),
                });
            };
            (p.raw_name.clone(), Arc::clone(&p.transport))
        };
        transport::get_prompt(transport.as_ref(), &raw_name, arguments, scope).await
    }

    /// Reads `uri` from the running `server`, which must have declared
    /// resources.
    pub async fn read_resource(
        &self,
        server: &str,
        uri: &str,
        scope: &CallScope,
    ) -> Result<Vec<ResourceContents>, McpError> {
        let transport = {
            let index = self.index.load();
            let running = index
                .servers
                .get(server)
                .ok_or_else(|| McpError::ServerDied {
                    server: server.to_string(),
                })?;
            if !running.resources {
                return Err(McpError::Unsupported {
                    server: server.to_string(),
                    feature: RESOURCES_FEATURE,
                });
            }
            Arc::clone(&running.transport)
        };
        resources::read_resource(transport.as_ref(), uri, scope).await
    }

    pub async fn shutdown(&self) {
        let (ack_tx, ack_rx) = flume::bounded(1);
        self.send(McpCommand::Shutdown { ack: ack_tx });
        let finished = futures_lite::future::or(
            async {
                let _ = ack_rx.recv_async().await;
                true
            },
            async {
                smol::Timer::after(MCP_SHUTDOWN_TIMEOUT).await;
                false
            },
        )
        .await;
        if !finished {
            warn!("MCP shutdown timed out after {MCP_SHUTDOWN_TIMEOUT:?}");
        }
    }
}

/// Returns as soon as the config is read, so nothing with a screen waits on a
/// slow `initialize`. Await `McpHandle::ready` before touching the tool index.
pub async fn start(cwd: &Path) -> (Option<McpHandle>, McpConfigErrors) {
    tracing::info!(cwd = %cwd.display(), "starting MCP");
    let cwd = cwd.to_owned();
    let (config, config_errors) = smol::unblock(move || load_config(&cwd)).await;
    (start_with_config(config), config_errors)
}

/// `start` for callers with no frame to protect, who want the tools up front.
pub async fn start_connected(cwd: &Path) -> (Option<McpHandle>, McpConfigErrors) {
    let (handle, config_errors) = start(cwd).await;
    if let Some(handle) = &handle {
        handle.ready().await;
    }
    (handle, config_errors)
}

pub async fn start_global_connected(cwd: &Path) -> (Option<McpHandle>, McpConfigErrors) {
    let cwd = cwd.to_owned();
    let (config, config_errors) = smol::unblock(move || load_global_config(&cwd)).await;
    let handle = start_with_config(config);
    if let Some(handle) = &handle {
        handle.ready().await;
    }
    (handle, config_errors)
}

pub async fn start_global(cwd: &Path) -> (Option<McpHandle>, McpConfigErrors) {
    let cwd = cwd.to_owned();
    let (config, config_errors) = smol::unblock(move || load_global_config(&cwd)).await;
    (start_with_config(config), config_errors)
}

/// `start` plus servers declared at runtime. `mcp.toml` wins on name, so a
/// runtime server can never swap out the credentials the user configured or
/// revive one they disabled.
pub async fn start_with_extra(
    cwd: &Path,
    extra: Vec<(String, RawTransport)>,
) -> (Option<McpHandle>, McpConfigErrors) {
    let owned_cwd = cwd.to_owned();
    let (mut config, config_errors) = smol::unblock(move || load_config(&owned_cwd)).await;
    merge_runtime_servers(&mut config, extra);
    (start_with_config(config), config_errors)
}

pub async fn pending_startup_trust(
    cwd: &Path,
    extra: Vec<(String, RawTransport)>,
) -> (Vec<String>, McpConfigErrors) {
    let owned_cwd = cwd.to_owned();
    let (mut config, config_errors) = smol::unblock(move || load_config(&owned_cwd)).await;
    merge_runtime_servers(&mut config, extra);
    let inner = parse_entries(config, StateDir::resolve().ok());
    let pending = inner
        .entries
        .into_iter()
        .filter(|entry| entry.status == McpServerStatus::AwaitingTrust)
        .map(|entry| entry.name)
        .collect();
    (pending, config_errors)
}

fn merge_runtime_servers(config: &mut McpConfig, extra: Vec<(String, RawTransport)>) {
    for (name, transport) in extra {
        let runtime_name = name.clone();
        match config.mcp.entry(name) {
            Entry::Vacant(slot) => {
                slot.insert(RawServerConfig::runtime(transport));
                config
                    .sources
                    .insert(runtime_name, McpConfigSource::Runtime);
            }
            Entry::Occupied(slot) => {
                warn!(server = slot.key(), "runtime MCP server already configured");
            }
        }
    }
}

pub fn start_with_config(config: McpConfig) -> Option<McpHandle> {
    start_with_config_and_state(config, StateDir::resolve().ok())
}

fn start_with_config_and_state(
    config: McpConfig,
    state_dir: Option<StateDir>,
) -> Option<McpHandle> {
    if config.is_empty() {
        tracing::info!("no MCP servers configured, skipping");
        return None;
    }

    let inner = parse_entries(config, state_dir);

    let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
    let index: Arc<ArcSwap<ToolIndex>> = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
    publish(&inner, &index, &snapshot);

    let (cmd_tx, cmd_rx) = flume::unbounded();
    let (ready_tx, ready_rx) = flume::bounded(0);
    let handle = McpHandle {
        cmd_tx,
        index: Arc::clone(&index),
        snapshot: Arc::clone(&snapshot),
        ready_rx,
    };

    info!(total = inner.entries.len(), "MCP servers connecting");

    smol::spawn(run(inner, index, snapshot, cmd_rx, ready_tx)).detach();
    Some(handle)
}

/// Connect results ride the same loop as commands, so a `Shutdown` arriving
/// mid-connect never waits for a slow `initialize`.
enum Step {
    Connected(Connect),
    Relisted(Relisted),
    Command(McpCommand),
    /// Every handle is gone.
    Closed,
}

/// A finished connect, addressed by name and epoch because a reload reorders
/// entries and may replace the one it was started for.
struct Connect {
    server: String,
    epoch: u64,
    result: Result<StartResult, McpError>,
}

/// Pending connects by server. Held, not detached: dropping one drops the
/// transport it owns, which kills the child process group it just spawned.
type Connects = HashMap<String, smol::Task<()>>;

async fn run(
    mut inner: McpManagerInner,
    index: Arc<ArcSwap<ToolIndex>>,
    snapshot: Arc<ArcSwap<McpSnapshot>>,
    cmd_rx: flume::Receiver<McpCommand>,
    ready_tx: flume::Sender<Infallible>,
) {
    let (connected_tx, connected_rx) = flume::unbounded();
    let (relisted_tx, relisted_rx) = flume::unbounded();
    let mut connects = spawn_connects(&inner, STARTUP_EPOCH, &connected_tx);
    let mut ready = Some(ready_tx);

    let mut ack: Option<flume::Sender<()>> = None;
    loop {
        release_ready(&inner, &mut ready);
        for entry in &mut inner.entries {
            entry.watch_lists(&relisted_tx);
        }
        let step = futures_lite::future::or(
            async {
                match connected_rx.recv_async().await {
                    Ok(connect) => Step::Connected(connect),
                    // `run` keeps a sender for reloads, so this never closes.
                    Err(_) => futures_lite::future::pending().await,
                }
            },
            futures_lite::future::or(
                async {
                    match relisted_rx.recv_async().await {
                        Ok(relisted) => Step::Relisted(relisted),
                        // `run` keeps a sender for new watches, so this never closes.
                        Err(_) => futures_lite::future::pending().await,
                    }
                },
                async {
                    match cmd_rx.recv_async().await {
                        Ok(cmd) => Step::Command(cmd),
                        Err(_) => Step::Closed,
                    }
                },
            ),
        )
        .await;

        match step {
            Step::Connected(connect) => {
                // A toggle, reconnect or reload that ran mid-connect owns the
                // entry now, so a late result must not resurrect it. Dropping
                // it kills the transport it carries.
                if let Some(entry) = inner.entries.iter_mut().find(|entry| {
                    entry.name == connect.server
                        && entry.epoch == connect.epoch
                        && entry.status == McpServerStatus::Connecting
                }) {
                    let _ = apply_start_result(entry, connect.result, "start");
                }
            }
            Step::Relisted(relisted) => {
                if !apply_relisted(&mut inner, relisted) {
                    continue;
                }
            }
            Step::Command(McpCommand::Reload { config }) => {
                handle_reload(&mut inner, &mut connects, &connected_tx, config).await;
            }
            Step::Command(McpCommand::Toggle { server, enabled }) => {
                handle_toggle(&mut inner, &server, enabled).await;
            }
            Step::Command(McpCommand::Reconnect { server }) => {
                handle_reconnect(&mut inner, &server).await;
            }
            Step::Command(McpCommand::TrustOnce { server }) => {
                handle_trust(&mut inner, &server, false).await;
            }
            Step::Command(McpCommand::TrustProject { server }) => {
                handle_trust(&mut inner, &server, true).await;
            }
            Step::Command(McpCommand::Reject { server }) => {
                handle_reject(&mut inner, &server).await;
            }
            Step::Command(McpCommand::Shutdown { ack: tx }) => {
                ack = Some(tx);
                break;
            }
            Step::Closed => break,
        }
        inner.generation += 1;
        publish(&inner, &index, &snapshot);
    }
    drop(connects);
    shutdown_all(&mut inner).await;
    inner.generation += 1;
    publish(&inner, &index, &snapshot);
    if let Some(tx) = ack {
        let _ = tx.try_send(());
    }
}

/// Nothing left in `Connecting` means every server landed and the last publish
/// already carried its tools, so waiters can go.
fn release_ready(inner: &McpManagerInner, ready: &mut Option<flume::Sender<Infallible>>) {
    if ready.is_none()
        || inner
            .entries
            .iter()
            .any(|e| e.status == McpServerStatus::Connecting)
    {
        return;
    }
    drop(ready.take());
    info!(
        running = inner
            .entries
            .iter()
            .filter(|e| e.transport.is_some())
            .count(),
        total = inner.entries.len(),
        "MCP servers initialized"
    );
}

async fn handle_toggle(inner: &mut McpManagerInner, server_name: &str, enabled: bool) {
    if let Some((path, source)) = inner
        .entries
        .iter()
        .find(|e| e.name == server_name)
        .map(|e| (e.origin.clone(), e.source))
        && source != McpConfigSource::Runtime
        && !path.as_os_str().is_empty()
    {
        spawn_persist_enabled(path, server_name.to_owned(), enabled);
    }

    if enabled {
        let unpinned_url = inner
            .entries
            .iter()
            .find(|entry| entry.name == server_name)
            .and_then(|entry| entry.config.as_ref())
            .and_then(|config| match &config.transport {
                Transport::Http { url, resolved, .. } if resolved.is_empty() => Some(url.clone()),
                _ => None,
            });
        if let Some(url) = unpinned_url {
            let resolution = smol::unblock(move || config::resolve_url_addresses(&url)).await;
            let Some(entry) = inner
                .entries
                .iter_mut()
                .find(|entry| entry.name == server_name)
            else {
                return;
            };
            match resolution {
                Ok(addresses) => {
                    if let Some(ServerConfig {
                        transport: Transport::Http { resolved, .. },
                        ..
                    }) = entry.config.as_mut()
                    {
                        *resolved = addresses;
                    }
                }
                Err(error) => {
                    entry.status = if entry.trust.is_some() {
                        McpServerStatus::AwaitingTrust
                    } else {
                        McpServerStatus::Failed(format!("cannot resolve MCP URL: {error}"))
                    };
                    warn!(server = server_name, %error, "MCP server has no pinned address");
                    return;
                }
            }
        }
        if let Err(e) = refresh_server(inner, server_name).await {
            warn!(server = %server_name, error = %e, "MCP server refresh failed");
        }
    } else if let Some(entry) = inner.entries.iter_mut().find(|e| e.name == server_name) {
        entry.clear_connection().await;
        entry.status = McpServerStatus::Disabled;
    }

    info!(server = server_name, enabled, "MCP toggle complete");
}

/// Restart the server with its stored config. Fresh OAuth tokens are picked up
/// from storage by the transport, so no credentials travel through the command.
async fn handle_reconnect(inner: &mut McpManagerInner, server_name: &str) {
    let Some(entry) = inner.entries.iter().find(|e| e.name == server_name) else {
        warn!(server = server_name, "reconnect for unknown server");
        return;
    };
    if entry.status == McpServerStatus::Disabled {
        info!(
            server = server_name,
            "ignoring reconnect for disabled server"
        );
        return;
    }
    if let Err(e) = refresh_server(inner, server_name).await {
        warn!(server = %server_name, error = %e, "reconnect failed");
    }
    info!(server = server_name, "MCP reconnect complete");
}

async fn handle_trust(inner: &mut McpManagerInner, server_name: &str, persist: bool) {
    let Some(index) = inner
        .entries
        .iter()
        .position(|entry| entry.name == server_name)
    else {
        warn!(server = server_name, "trust for unknown MCP server");
        return;
    };
    if inner.entries[index].status == McpServerStatus::Disabled {
        info!(
            server = server_name,
            "ignoring trust for disabled MCP server"
        );
        return;
    }
    let Some(trust) = inner.entries[index].trust.clone() else {
        info!(
            server = server_name,
            "MCP server does not require project trust"
        );
        return;
    };

    let url = inner.entries[index]
        .config
        .as_ref()
        .and_then(|config| transport_url(&config.transport));
    if let Some(url) = url {
        let resolve_url = url.clone();
        let resolving_server = server_name.to_owned();
        let addresses = match smol::unblock(move || {
            config::resolve_url_addresses(&resolve_url).map_err(|error| McpError::StartFailed {
                server: resolving_server,
                reason: format!("cannot resolve reviewed URL: {error}"),
            })
        })
        .await
        {
            Ok(addresses) => addresses,
            Err(error) => {
                warn!(server = server_name, %error, "MCP trust requires a resolved address");
                return;
            }
        };
        if let Some(ServerConfig {
            transport: Transport::Http { resolved, .. },
            ..
        }) = inner.entries[index].config.as_mut()
        {
            *resolved = addresses;
        }
    }

    if persist {
        let (Some(state_dir), Some(project)) = (inner.state_dir.clone(), trust.project) else {
            warn!(
                server = server_name,
                "cannot persist MCP trust without state and canonical project directories"
            );
            return;
        };
        let server = server_name.to_string();
        let digest = trust.config_digest;
        let result =
            smol::unblock(move || trust_project(&state_dir, &project, &server, &digest)).await;
        if let Err(error) = result {
            warn!(server = server_name, error = %error, "failed to persist MCP project trust");
            return;
        }
    } else {
        inner.entries[index].trusted_once = true;
    }
    inner.entries[index].trust_rejected = false;

    if let Err(error) = refresh_server(inner, server_name).await {
        warn!(server = server_name, error = %error, "trusted MCP server start failed");
    }
}

async fn handle_reject(inner: &mut McpManagerInner, server_name: &str) {
    let Some(index) = inner
        .entries
        .iter()
        .position(|entry| entry.name == server_name)
    else {
        warn!(server = server_name, "reject for unknown MCP server");
        return;
    };
    inner.entries[index].trusted_once = false;
    inner.entries[index].trust_rejected = true;

    if let Some(trust) = inner.entries[index].trust.clone()
        && let (Some(state_dir), Some(project)) = (inner.state_dir.clone(), trust.project)
    {
        let server = server_name.to_string();
        if let Err(error) =
            smol::unblock(move || revoke_project_trust(&state_dir, &project, &server)).await
        {
            warn!(server = server_name, error = %error, "failed to revoke MCP project trust");
        }
    }
    handle_toggle(inner, server_name, false).await;
}

/// Logs names and counts only: a definition may carry secrets from `{env:NAME}`.
async fn handle_reload(
    inner: &mut McpManagerInner,
    connects: &mut Connects,
    connected_tx: &flume::Sender<Connect>,
    config: McpConfig,
) {
    if config.local_execution != inner.local_execution {
        warn!(
            servers = config.mcp.len(),
            "ignoring MCP reload loaded under another execution policy"
        );
        return;
    }
    let epoch = inner.generation + 1;
    let state_dir = inner.state_dir.clone();
    let fresh = smol::unblock(move || parse_entries(config, state_dir).entries).await;
    let mut current: HashMap<String, ServerEntry> = inner
        .entries
        .drain(..)
        .map(|entry| (entry.name.clone(), entry))
        .collect();
    let (mut added, mut changed, mut unchanged) = (0, 0, 0);

    for mut entry in fresh {
        match current.remove(&entry.name) {
            Some(running) if same_definition(&running, &entry) => {
                unchanged += 1;
                inner.entries.push(running);
                continue;
            }
            Some(mut running) => {
                changed += 1;
                connects.remove(&running.name);
                running.clear_connection().await;
                inherit_session_trust(&mut entry, &running);
                info!(server = %entry.name, "MCP server changed on reload");
            }
            None => {
                added += 1;
                info!(server = %entry.name, "MCP server added on reload");
            }
        }
        entry.epoch = epoch;
        inner.entries.push(entry);
    }

    let mut removed = 0;
    for (name, mut running) in current {
        if running.source == McpConfigSource::Runtime {
            inner.entries.push(running);
            continue;
        }
        removed += 1;
        connects.remove(&name);
        running.clear_connection().await;
        info!(server = %name, "MCP server removed on reload");
    }
    inner.entries.sort_by(|a, b| a.name.cmp(&b.name));
    connects.extend(spawn_connects(inner, epoch, connected_tx));

    let awaiting_trust = inner
        .entries
        .iter()
        .filter(|entry| entry.epoch == epoch && entry.status == McpServerStatus::AwaitingTrust)
        .count();
    info!(
        added,
        changed, removed, unchanged, awaiting_trust, "MCP config reloaded"
    );
}

/// Whether `fresh` runs exactly what `running` runs. The authority digest
/// covers provenance and every reviewed transport field, so only what review
/// leaves out is compared besides. Disabled-ness, not status, is the enabled
/// flag, so the file `/mcp` just wrote reads back as unchanged.
fn same_definition(running: &ServerEntry, fresh: &ServerEntry) -> bool {
    let unreviewed = |entry: &ServerEntry| entry.config.as_ref().map(|config| config.timeout);
    running.authority_digest == fresh.authority_digest
        && unreviewed(running) == unreviewed(fresh)
        && (running.status == McpServerStatus::Disabled)
            == (fresh.status == McpServerStatus::Disabled)
}

/// A one-off trust covers the reviewed definition, so it outlives a reload
/// that changed only what review leaves out. An HTTP server still needs the
/// pinned address startup would demand.
fn inherit_session_trust(fresh: &mut ServerEntry, running: &ServerEntry) {
    if !running.trusted_once || running.authority_digest != fresh.authority_digest {
        return;
    }
    fresh.trusted_once = true;
    let unpinned = matches!(
        fresh.config.as_ref().map(|config| &config.transport),
        Some(Transport::Http { resolved, .. }) if resolved.is_empty()
    );
    if fresh.status == McpServerStatus::AwaitingTrust && !unpinned {
        fresh.status = McpServerStatus::Connecting;
    }
}

async fn shutdown_all(inner: &mut McpManagerInner) {
    for entry in &mut inner.entries {
        entry.clear_connection().await;
        if !matches!(
            entry.status,
            McpServerStatus::Disabled | McpServerStatus::AwaitingTrust
        ) {
            entry.status = McpServerStatus::Failed("shutdown".into());
        }
    }
    info!("MCP command loop shutting down");
}

/// Tear the old transport down and wipe tools/prompts *before* starting the new one. That way
/// a failed start leaves the entry empty instead of holding zombie tool references into a dead
/// transport.
async fn refresh_server(inner: &mut McpManagerInner, server_name: &str) -> Result<(), McpError> {
    let Some(idx) = inner.entries.iter().position(|e| e.name == server_name) else {
        return Err(McpError::Config(format!("unknown server '{server_name}'")));
    };

    if !entry_has_startup_trust(&inner.entries[idx], inner.state_dir.as_ref()) {
        let entry = &mut inner.entries[idx];
        entry.clear_connection().await;
        entry.status = McpServerStatus::AwaitingTrust;
        return Ok(());
    }

    let config = inner.entries[idx]
        .config
        .clone()
        .ok_or_else(|| McpError::Config(format!("server '{server_name}' has no config")))?;

    {
        let entry = &mut inner.entries[idx];
        entry.status = McpServerStatus::Connecting;
        entry.clear_connection().await;
    }

    let result = start_server(
        &config,
        entry_has_startup_trust(&inner.entries[idx], inner.state_dir.as_ref()),
    )
    .await;
    apply_start_result(&mut inner.entries[idx], result, "refresh")?;
    info!(
        server = server_name,
        tools = inner.entries[idx].tools.len(),
        "MCP server refreshed"
    );
    Ok(())
}

fn entry_has_startup_trust(entry: &ServerEntry, state_dir: Option<&StateDir>) -> bool {
    let Some(trust) = &entry.trust else {
        return true;
    };
    if entry.trust_rejected {
        return false;
    }
    entry.trusted_once || stored_project_trust(state_dir, trust, &entry.name)
}

fn stored_project_trust(
    state_dir: Option<&StateDir>,
    trust: &ProjectTrust,
    server_name: &str,
) -> bool {
    let (Some(state_dir), Some(project)) = (state_dir, trust.project.as_deref()) else {
        return false;
    };
    match is_project_trusted(state_dir, project, server_name, &trust.config_digest) {
        Ok(trusted) => trusted,
        Err(error) => {
            warn!(server = server_name, error = %error, "failed to read MCP project trust");
            false
        }
    }
}

fn status_from_err(e: &McpError) -> McpServerStatus {
    match e {
        McpError::HttpError {
            status: 401,
            reason,
            ..
        } => McpServerStatus::NeedsAuth {
            url: Some(reason.clone()),
        },
        McpError::Refused { .. } => McpServerStatus::Refused,
        _ => McpServerStatus::Failed(e.to_string()),
    }
}

struct StartResult {
    transport: Arc<dyn McpTransport>,
    discovery: ServerDiscovery,
    tool_infos: Vec<ToolInfo>,
    prompt_infos: Vec<PromptInfo>,
    resources: ResourceLists,
}

async fn start_server(config: &ServerConfig, trusted: bool) -> Result<StartResult, McpError> {
    let transport: Arc<dyn McpTransport> = match &config.transport {
        Transport::Stdio {
            program,
            args,
            environment,
        } => Arc::new(StdioTransport::spawn(
            &config.name,
            program,
            args,
            environment,
            config.timeout,
            config.local_execution,
            trusted,
        )?),
        Transport::Http {
            url,
            headers,
            resolved,
            ..
        } => Arc::new(HttpTransport::new(
            &config.name,
            url,
            headers,
            resolved,
            config.timeout,
            caudra_storage::StateDir::resolve().ok(),
        )?),
    };
    let discovery = era::connect(transport.as_ref(), config.timeout).await?;
    let capabilities = &discovery.capabilities;
    // Asymmetric on purpose: sloppy servers omit `capabilities` yet serve
    // tools/list fine, so always ask (fatal only when tools were declared).
    // Prompts only when declared: undeclared endpoints may answer junk,
    // and junk must not take down the server's tools.
    let tool_infos = match transport::list_tools(transport.as_ref()).await {
        Ok(tools) => tools,
        Err(e) if capabilities.tools.is_none() => {
            warn!(server = config.name, error = %e, "tools/list failed; server declared no tools");
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    let prompt_infos = if capabilities.prompts.is_some() {
        transport::list_prompts(transport.as_ref()).await?
    } else {
        Vec::new()
    };
    // Resources are an extra: a broken listing costs the server its
    // resources, never its tools.
    let resources = if capabilities.resources.is_some() {
        resources::list_all(transport.as_ref())
            .await
            .unwrap_or_else(|error| {
                warn!(server = config.name, %error, "MCP resources/list failed");
                ResourceLists::default()
            })
    } else {
        ResourceLists::default()
    };
    info!(
        server = config.name,
        server_name = discovery.server_info.name,
        server_version = discovery.server_info.version,
        protocol_version = discovery.protocol_version,
        kind = ?discovery.kind(),
        tool_count = tool_infos.len(),
        prompt_count = prompt_infos.len(),
        resource_count = resources.resources.len(),
        template_count = resources.templates.len(),
        "MCP server initialized"
    );
    Ok(StartResult {
        transport,
        discovery,
        tool_infos,
        prompt_infos,
        resources,
    })
}

fn parse_entries(config: McpConfig, state_dir: Option<StateDir>) -> McpManagerInner {
    let local_execution = config.local_execution;
    let origins = config.origins;
    let sources = config.sources;
    let project_root = config.project_root;
    let mut entries = Vec::with_capacity(config.mcp.len());

    for (name, raw) in config.mcp {
        let transport_kind = transport_kind(&raw.transport);
        let origin = origins.get(&name).cloned().unwrap_or_default();
        let source = sources.get(&name).copied().unwrap_or_default();
        let review = review_summary(&raw.transport, source);
        let config_digest = security_digest(&raw);
        let authority_digest = canonical_json_sha256(&json!({
            "source": match source {
                McpConfigSource::Global => "global",
                McpConfigSource::Project => "project",
                McpConfigSource::Runtime => "runtime",
            },
            "config": config_digest,
        }));
        let resolved = resolve_http_addresses(&raw.transport);
        let resolution_failed = resolved.is_err();
        let resolved_risk = resolved
            .as_ref()
            .map_or(true, |addresses| addresses.iter().copied().any(risky_ip));
        let remote_stdio = local_execution == LocalExecutionPolicy::RemoteLocal
            && matches!(raw.transport, RawTransport::Stdio(_));
        let trust = (remote_stdio
            || (source == McpConfigSource::Project
                && (requires_project_trust(&raw.transport) || resolved_risk)))
            .then(|| ProjectTrust {
                project: if remote_stdio {
                    None
                } else {
                    project_root.clone()
                },
                config_digest,
            });
        let disabled = !raw.enabled;
        let parsed = parse_server(name.clone(), raw).map(|mut server| {
            server.local_execution = local_execution;
            server
        });
        let (config, status) = match parsed {
            Ok(mut sc) if disabled => {
                if let Transport::Http {
                    resolved: pinned, ..
                } = &mut sc.transport
                {
                    *pinned = resolved.unwrap_or_default();
                }
                (Some(sc), McpServerStatus::Disabled)
            }
            Ok(sc) if trust.is_some() && resolution_failed => {
                (Some(sc), McpServerStatus::AwaitingTrust)
            }
            Ok(mut sc)
                if trust.as_ref().is_some_and(|trust| {
                    !stored_project_trust(state_dir.as_ref(), trust, &name)
                }) =>
            {
                if let Transport::Http {
                    resolved: pinned, ..
                } = &mut sc.transport
                {
                    *pinned = resolved.unwrap_or_default();
                }
                (Some(sc), McpServerStatus::AwaitingTrust)
            }
            Ok(mut sc) => {
                if let Transport::Http {
                    resolved: pinned, ..
                } = &mut sc.transport
                {
                    *pinned = resolved.unwrap_or_default();
                }
                (Some(sc), McpServerStatus::Connecting)
            }
            Err(e) => {
                warn!(server = %name, error = %e, "invalid MCP server config");
                (None, McpServerStatus::Failed(e.to_string()))
            }
        };
        entries.push(ServerEntry {
            name,
            config,
            transport_kind,
            origin,
            source,
            review,
            authority_digest,
            trust,
            trusted_once: false,
            trust_rejected: false,
            status,
            transport: None,
            discovery: None,
            list_watch: None,
            paused_runs: Arc::default(),
            tools: Vec::new(),
            prompts: Vec::new(),
            resources: ResourceLists::default(),
            epoch: STARTUP_EPOCH,
        });
    }

    // Config maps are unordered; a stable order keeps the tool_search
    // catalog and tools array byte-identical across runs (prompt cache).
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    McpManagerInner {
        entries,
        state_dir,
        generation: 0,
        local_execution,
    }
}

/// One task per enabled server built in `epoch`, each reporting back as it
/// lands, so `run` publishes a fast server's tools without waiting for the slowest.
fn spawn_connects(inner: &McpManagerInner, epoch: u64, tx: &flume::Sender<Connect>) -> Connects {
    inner
        .entries
        .iter()
        .filter(|e| e.epoch == epoch && e.status == McpServerStatus::Connecting)
        .filter(|e| entry_has_startup_trust(e, inner.state_dir.as_ref()))
        .filter_map(|e| e.config.clone())
        .map(|config| {
            let tx = tx.clone();
            let server = config.name.clone();
            let task = smol::spawn(async move {
                let result = start_server(&config, true).await;
                let _ = tx
                    .send_async(Connect {
                        server: config.name,
                        epoch,
                        result,
                    })
                    .await;
            });
            (server, task)
        })
        .collect()
}

fn apply_start_result(
    entry: &mut ServerEntry,
    result: Result<StartResult, McpError>,
    action: &'static str,
) -> Result<(), McpError> {
    match result {
        Ok(start) => {
            entry.populate(start);
            Ok(())
        }
        Err(e) => {
            entry.status = status_from_err(&e);
            if !matches!(entry.status, McpServerStatus::NeedsAuth { .. }) {
                warn!(server = %entry.name, action, error = %e, "MCP server start failed");
            }
            Err(e)
        }
    }
}

/// The only place read-side state is updated. Every mutation in the command loop ends here.
fn publish(inner: &McpManagerInner, index: &ArcSwap<ToolIndex>, snapshot: &ArcSwap<McpSnapshot>) {
    let mut tools = HashMap::new();
    let mut prompts = HashMap::new();
    let mut descriptors: Vec<ToolDescriptor> = Vec::new();
    let mut server_infos = Vec::with_capacity(inner.entries.len());
    let mut prompt_infos = Vec::new();
    let mut servers = HashMap::new();
    let mut resource_offers = Vec::new();
    let mut pids = Vec::new();

    for entry in &inner.entries {
        let url = entry
            .config
            .as_ref()
            .and_then(|c| transport_url(&c.transport));
        let oauth = entry
            .config
            .as_ref()
            .and_then(|c| transport_oauth(&c.transport));
        let resolved_addresses =
            entry
                .config
                .as_ref()
                .map_or_else(Vec::new, |config| match &config.transport {
                    Transport::Http { resolved, .. } => resolved.clone(),
                    Transport::Stdio { .. } => Vec::new(),
                });

        let kind = entry.kind();
        if let Some(ref transport) = entry.transport
            && entry.status != McpServerStatus::Disabled
        {
            for t in &entry.tools {
                let contract = canonical_json_sha256(&json!({
                    "name": t.raw_name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                }));
                tools.insert(
                    Arc::clone(&t.qualified_name),
                    ToolRef {
                        qualified_name: Arc::clone(&t.qualified_name),
                        raw_name: t.raw_name.clone(),
                        subject: PermissionSubject::Mcp {
                            server: entry.name.clone(),
                            authority: entry.authority_digest.clone(),
                            tool: t.raw_name.clone(),
                            contract,
                        },
                        generation: inner.generation,
                        transport: Arc::clone(transport),
                        meta: Arc::clone(&t.meta),
                        kind,
                        paused_runs: Arc::clone(&entry.paused_runs),
                    },
                );
                descriptors.push(ToolDescriptor {
                    qualified_name: Arc::clone(&t.qualified_name),
                    server: entry.name.clone(),
                    view: ToolView {
                        name: t.raw_name.clone(),
                        title: t.meta.title().map(str::to_owned),
                        description: t.description.clone(),
                        input_schema: t.input_schema.clone(),
                        output_schema: t.meta.output_schema.clone(),
                        search_only: false,
                    },
                });
            }
            let capabilities = entry
                .discovery
                .as_ref()
                .map(|discovery| &discovery.capabilities);
            let completions = capabilities.is_some_and(|c| c.completions);
            let offers_resources = capabilities.is_some_and(|c| c.resources.is_some());
            for p in &entry.prompts {
                prompts.insert(
                    p.qualified_name.clone(),
                    PromptRef {
                        raw_name: p.raw_name.clone(),
                        transport: Arc::clone(transport),
                        completions,
                    },
                );
                prompt_infos.push(p.to_info(&entry.name, completions));
            }
            servers.insert(
                entry.name.clone(),
                ServerRef {
                    transport: Arc::clone(transport),
                    resources: offers_resources,
                    completions,
                },
            );
            if offers_resources {
                resource_offers.push(McpServerResources {
                    server: entry.name.clone(),
                    lists: entry.resources.clone(),
                    completions,
                });
            }
            pids.extend(transport.child_pids());
        }

        server_infos.push(McpServerInfo {
            name: entry.name.clone(),
            transport_kind: entry.transport_kind,
            tool_count: entry.tools.len(),
            prompt_count: entry.prompts.len(),
            status: entry.status.clone(),
            config_path: entry.origin.clone(),
            url,
            oauth,
            resolved_addresses,
            review: entry.review.clone(),
            kind,
            discovery: entry.discovery.clone(),
        });
    }

    index.store(Arc::new(ToolIndex {
        tools,
        prompts,
        servers,
        descriptors: descriptors.into(),
    }));
    snapshot.store(Arc::new(McpSnapshot {
        infos: server_infos,
        prompts: prompt_infos,
        resources: resource_offers,
        pids,
        generation: inner.generation,
    }));
}

/// Session for dispatch-level tests outside this module, built through the
/// real `publish` path so it can't drift from production index construction.
#[cfg(any(test, feature = "test-support"))]
pub fn stub_session(tools: &[(&str, &str)]) -> McpSession {
    stub_session_replying(tools, None)
}

/// [`stub_session`] whose server answers every `tools/call` with `reply`.
#[cfg(any(test, feature = "test-support"))]
pub fn stub_session_replying(tools: &[(&str, &str)], reply: Option<Value>) -> McpSession {
    let tools = tools
        .iter()
        .map(|(qualified, description)| stub_tool(qualified, description, McpToolMeta::default()))
        .collect();
    stub_session_from(None, tools, reply, None, Vec::new())
}

/// A stub session over a server named `stub` that declared resources, lists
/// `lists`, and answers every `resources/read` with `read`.
#[cfg(any(test, feature = "test-support"))]
pub fn resource_stub_session(lists: ResourceLists, read: Value) -> McpSession {
    let discovery = ServerDiscovery {
        capabilities: protocol::ServerCapabilities {
            resources: Some(protocol::ListCapability::default()),
            ..Default::default()
        },
        ..Default::default()
    };
    stub_session_from(
        Some(discovery),
        Vec::new(),
        None,
        Some((lists, read)),
        Vec::new(),
    )
}

/// A stub session over a server named `stub` that declared resources and
/// completions: it lists `lists`, offers `prompt` taking `arguments`, and
/// completes anything with `values`.
#[cfg(any(test, feature = "test-support"))]
pub fn completing_stub_session(
    lists: ResourceLists,
    prompt: &str,
    arguments: &[&str],
    values: &[&str],
) -> McpSession {
    let discovery = ServerDiscovery {
        capabilities: protocol::ServerCapabilities {
            resources: Some(protocol::ListCapability::default()),
            prompts: Some(protocol::ListCapability::default()),
            completions: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let info = protocol::PromptInfo {
        name: prompt.into(),
        description: None,
        arguments: arguments
            .iter()
            .map(|name| protocol::PromptArgument {
                name: (*name).into(),
                description: None,
                required: true,
            })
            .collect(),
    };
    stub_session_from(
        Some(discovery),
        Vec::new(),
        Some(json!({ "completion": { "values": values } })),
        Some((lists, Value::Null)),
        vec![McpPromptDef::from_info("stub", info)],
    )
}

/// A stub session over a server that calls itself `server_name`, whose
/// tools are `(qualified name, annotated read-only)` and whose every
/// `tools/call` answers `result`.
#[cfg(any(test, feature = "test-support"))]
pub fn recognised_stub_session(
    server_name: &str,
    tools: &[(&str, bool)],
    result: Value,
) -> McpSession {
    let discovery = ServerDiscovery {
        server_info: protocol::ServerInfo {
            name: server_name.into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let tools = tools
        .iter()
        .map(|(qualified, read_only)| {
            let meta = McpToolMeta {
                annotations: ToolAnnotations {
                    read_only_hint: Some(*read_only),
                    ..Default::default()
                },
                ..Default::default()
            };
            stub_tool(qualified, "", meta)
        })
        .collect();
    stub_session_from(Some(discovery), tools, Some(result), None, Vec::new())
}

#[cfg(any(test, feature = "test-support"))]
fn stub_tool(qualified: &str, description: &str, meta: McpToolMeta) -> McpToolDef {
    McpToolDef {
        qualified_name: Arc::from(qualified),
        raw_name: qualified
            .split_once(SEPARATOR)
            .map_or(qualified, |(_, raw)| raw)
            .into(),
        description: description.into(),
        input_schema: json!({}),
        meta: Arc::new(meta),
    }
}

#[cfg(any(test, feature = "test-support"))]
fn stub_session_from(
    discovery: Option<ServerDiscovery>,
    tools: Vec<McpToolDef>,
    result: Option<Value>,
    resources: Option<(ResourceLists, Value)>,
    prompts: Vec<McpPromptDef>,
) -> McpSession {
    let (resources, read) = resources.unzip();
    let entry = ServerEntry {
        name: "stub".into(),
        config: None,
        transport_kind: "stub",
        origin: PathBuf::new(),
        source: McpConfigSource::Runtime,
        review: McpReviewSummary {
            command: None,
            url: None,
            config_source: McpConfigSource::Runtime,
            environment_names: Vec::new(),
            header_names: Vec::new(),
        },
        authority_digest: "stub-authority".into(),
        trust: None,
        trusted_once: false,
        trust_rejected: false,
        status: McpServerStatus::Running,
        transport: Some(Arc::new(StubTransport {
            name: Arc::from("stub"),
            result,
            read,
        })),
        discovery,
        list_watch: None,
        paused_runs: Arc::default(),
        tools,
        prompts,
        resources: resources.unwrap_or_default(),
        epoch: STARTUP_EPOCH,
    };
    let inner = McpManagerInner {
        entries: vec![entry],
        state_dir: None,
        generation: 0,
        local_execution: LocalExecutionPolicy::Embedded,
    };
    let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
    let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
    publish(&inner, &index, &snapshot);
    McpSession::new(McpHandle {
        cmd_tx: flume::unbounded().0,
        index,
        snapshot,
        ready_rx: flume::bounded(0).1,
    })
}

#[cfg(test)]
pub(crate) fn tool_names(tools: &Value) -> Vec<&str> {
    tools
        .as_array()
        .expect("tools must be a JSON array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect()
}

/// Answers every `tools/call` and `completion/complete` with `result` and
/// every `resources/read` with `read`, and refuses everything else.
#[cfg(any(test, feature = "test-support"))]
struct StubTransport {
    name: Arc<str>,
    result: Option<Value>,
    read: Option<Value>,
}

#[cfg(any(test, feature = "test-support"))]
impl McpTransport for StubTransport {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        _params: Option<Value>,
        _scope: &'a CallScope,
    ) -> transport::BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(async move {
            let reply = match method {
                protocol::TOOLS_CALL_METHOD | protocol::COMPLETION_COMPLETE_METHOD => &self.result,
                protocol::RESOURCES_READ_METHOD => &self.read,
                _ => &None,
            };
            match reply {
                Some(reply) => Ok(reply.clone()),
                None => Err(McpError::UnknownTool {
                    name: method.into(),
                }),
            }
        })
    }
    fn send_notification<'a>(
        &'a self,
        _method: &'a str,
        _params: Option<Value>,
    ) -> transport::BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async { Ok(()) })
    }
    fn shutdown<'a>(&'a self) -> transport::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn server_name(&self) -> &Arc<str> {
        &self.name
    }
    fn transport_kind(&self) -> &'static str {
        "stub"
    }
}

fn transport_url(transport: &Transport) -> Option<String> {
    match transport {
        Transport::Http { url, .. } => Some(url.clone()),
        Transport::Stdio { .. } => None,
    }
}

fn transport_oauth(transport: &Transport) -> Option<OauthClientConfig> {
    match transport {
        Transport::Http { oauth, .. } => oauth.clone(),
        _ => None,
    }
}

fn spawn_persist_enabled(path: PathBuf, name: String, enabled: bool) {
    let log_name = name.clone();
    smol::spawn(async move {
        if let Err(e) = smol::unblock(move || config::persist_enabled(&path, &name, enabled)).await
        {
            warn!(error = %e, server = %log_name, "failed to persist MCP toggle");
        }
    })
    .detach();
}

#[cfg(unix)]
pub fn kill_process_groups(pids: &[u32]) {
    for &pid in pids {
        unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
pub fn kill_process_groups(_pids: &[u32]) {}

/// Dedup cache for qualified MCP tool names. The set is bounded (finite per session)
/// and `Arc<str>` means entries get freed when the cache drops, unlike the old `Box::leak`.
fn intern(name: String) -> Arc<str> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<str>>>> = OnceLock::new();
    let mut map = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = map.get(&name) {
        return Arc::clone(existing);
    }
    let arc: Arc<str> = Arc::from(name.as_str());
    map.insert(name, Arc::clone(&arc));
    arc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use async_lock::Mutex as AsyncMutex;
    use caudra_config::mcp::MCP_FILE;
    use caudra_storage::sessions::SessionDatabase;
    use caudra_storage::state::project_scope;
    use caudra_workspace::PlanRef;
    use config::{RawHttpFields, RawServerConfig, RawStdioFields, RawTransport};
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(unix)]
    use std::time::Instant;
    use test_case::test_case;

    use crate::mcp::inbound::Inbound;
    use crate::mcp::message::Notification;
    use crate::mcp::protocol::ServerInfo;

    const DEFAULT_TIMEOUT_MS: u64 = 30_000;
    const MISSING_PROGRAM: &str = "/nonexistent/definitely-not-here";
    const REPLACEMENT_DESCRIPTION: &str = "replacement contract";
    const REPLACEMENT_TOOL_NAME: &str = "srv.replacement";
    const RELOAD_SERVER: &str = "srv";
    const OTHER_SERVER: &str = "other";
    const FAKE_TOOL: &str = "tool";
    const SECRET_KEY: &str = "TOKEN";
    const SECRET_VARIABLE: &str = "CAUDRA_TEST_MCP_RELOAD_SECRET";
    const FIRST_SECRET: &str = "first-reload-secret";
    const ROTATED_SECRET: &str = "rotated-reload-secret";
    const CHANGED_ARGUMENT: &str = "--changed";
    const CHANGED_TIMEOUT_MS: u64 = 5_000;
    const ENV_FILE: &str = ".env";
    const ADDED_TOOL: &str = "added";
    const TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
    const LIST_CHANGE_BURST: usize = 3;
    const TOOL_TITLE: &str = "Run a script";
    const ANNOTATION_TITLE: &str = "Script runner";
    const EXECUTOR_SERVER_NAME: &str = "executor";
    const NEGOTIATED_PROTOCOL: &str = "2025-06-18";
    const INSTRUCTIONS: &str = "Search before you execute.";

    fn stdio_raw(cmd: &[&str]) -> RawServerConfig {
        RawServerConfig {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            transport: RawTransport::Stdio(RawStdioFields {
                command: cmd.iter().map(|s| s.to_string()).collect(),
                environment: HashMap::new(),
            }),
        }
    }

    fn http_raw(url: &str) -> RawServerConfig {
        RawServerConfig {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            transport: RawTransport::Http(RawHttpFields {
                url: url.into(),
                headers: HashMap::new(),
                oauth: None,
            }),
        }
    }

    fn make_config(entries: Vec<(&str, RawServerConfig)>) -> McpConfig {
        let mut mcp = HashMap::new();
        let mut origins = HashMap::new();
        let mut sources = HashMap::new();
        for (name, cfg) in entries {
            origins.insert(name.to_string(), PathBuf::from("/test/config.toml"));
            sources.insert(name.to_string(), McpConfigSource::Global);
            mcp.insert(name.to_string(), cfg);
        }
        McpConfig {
            mcp,
            origins,
            sources,
            ..Default::default()
        }
    }

    fn make_project_config(project: &Path, entries: Vec<(&str, RawServerConfig)>) -> McpConfig {
        let mut config = make_config(entries);
        config.origins.clear();
        config.project_root = Some(project.canonicalize().unwrap());
        for source in config.sources.values_mut() {
            *source = McpConfigSource::Project;
        }
        config
    }

    const TOOL_NAME: &str = "srv.tool";
    const PLAN_PATH: &str = "/tmp/active-plan.md";
    const PLAN_REFERENCE: &str = "plan_0123456789abcdef0123456789abcdef";

    /// Counts shutdowns, signals on `call_entered` the moment a `tools/call` begins, and holds
    /// the call inside `call_gate` until tests release it. That way tests can meet an in-flight
    /// RPC at a known point without polling.
    struct FakeTransport {
        name: Arc<str>,
        shutdowns: AtomicUsize,
        call_entered: flume::Sender<()>,
        call_entered_rx: flume::Receiver<()>,
        call_gate: AsyncMutex<()>,
    }

    impl FakeTransport {
        fn new() -> Arc<Self> {
            let (call_entered, call_entered_rx) = flume::bounded(1);
            Arc::new(Self {
                name: Arc::from("fake"),
                shutdowns: AtomicUsize::new(0),
                call_entered,
                call_entered_rx,
                call_gate: AsyncMutex::new(()),
            })
        }

        fn shutdowns(&self) -> usize {
            self.shutdowns.load(Ordering::SeqCst)
        }
    }

    impl McpTransport for FakeTransport {
        fn send_request<'a>(
            &'a self,
            method: &'a str,
            _params: Option<Value>,
            _scope: &'a CallScope,
        ) -> transport::BoxFuture<'a, Result<Value, McpError>> {
            Box::pin(async move {
                if method == "tools/call" {
                    let _ = self.call_entered.try_send(());
                    let _g = self.call_gate.lock().await;
                    Ok(json!({ "content": [{ "type": "text", "text": "ok" }] }))
                } else {
                    Ok(Value::Null)
                }
            })
        }
        fn send_notification<'a>(
            &'a self,
            _method: &'a str,
            _params: Option<Value>,
        ) -> transport::BoxFuture<'a, Result<(), McpError>> {
            Box::pin(async move { Ok(()) })
        }
        fn shutdown<'a>(&'a self) -> transport::BoxFuture<'a, ()> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
        fn server_name(&self) -> &Arc<str> {
            &self.name
        }
        fn transport_kind(&self) -> &'static str {
            "fake"
        }
    }

    fn fake_entry(name: &str, transport: Arc<dyn McpTransport>) -> ServerEntry {
        let qualified = intern(format!("{name}{SEPARATOR}tool"));
        ServerEntry {
            name: name.into(),
            config: None,
            transport_kind: "fake",
            origin: PathBuf::new(),
            source: McpConfigSource::Runtime,
            review: McpReviewSummary {
                command: None,
                url: None,
                config_source: McpConfigSource::Runtime,
                environment_names: Vec::new(),
                header_names: Vec::new(),
            },
            authority_digest: format!("fake-authority:{name}"),
            trust: None,
            trusted_once: false,
            trust_rejected: false,
            status: McpServerStatus::Running,
            transport: Some(transport),
            discovery: None,
            list_watch: None,
            paused_runs: Arc::default(),
            tools: vec![McpToolDef {
                qualified_name: qualified,
                raw_name: "tool".into(),
                description: String::new(),
                input_schema: json!({}),
                meta: Arc::default(),
            }],
            prompts: Vec::new(),
            resources: ResourceLists::default(),
            epoch: STARTUP_EPOCH,
        }
    }

    fn bad_stdio_config(name: &str) -> ServerConfig {
        ServerConfig {
            local_execution: LocalExecutionPolicy::Embedded,
            name: name.into(),
            timeout: Duration::from_secs(1),
            transport: Transport::Stdio {
                program: MISSING_PROGRAM.into(),
                args: vec![],
                environment: HashMap::new(),
            },
        }
    }

    /// Build `inner`, publish it into fresh `ArcSwap`s, and return a live `McpSession` pointing
    /// at the same state so tests can hit both the mutation and the read path.
    fn setup(entries: Vec<ServerEntry>) -> (McpManagerInner, McpSession) {
        let inner = McpManagerInner {
            entries,
            state_dir: None,
            generation: 0,
            local_execution: LocalExecutionPolicy::Embedded,
        };
        let handle = published(&inner);
        (inner, McpSession::new(handle))
    }

    /// A handle over what `inner` publishes, so tests read the same index and
    /// snapshot the command loop would hand out.
    fn published(inner: &McpManagerInner) -> McpHandle {
        let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
        let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
        publish(inner, &index, &snapshot);
        McpHandle {
            cmd_tx: flume::unbounded().0,
            index,
            snapshot,
            ready_rx: flume::bounded(0).1,
        }
    }

    #[test]
    fn bound_tool_keeps_reviewed_transport_and_contract_snapshot() {
        let original = FakeTransport::new();
        let original_transport: Arc<dyn McpTransport> = original.clone();
        let (mut inner, session) = setup(vec![fake_entry("srv", original_transport.clone())]);
        let binding = session.bind_tool(TOOL_NAME).unwrap();
        let reviewed_subject = binding.subject().clone();
        assert!(Arc::ptr_eq(&binding.tool.transport, &original_transport));

        let replacement = FakeTransport::new();
        let replacement_transport: Arc<dyn McpTransport> = replacement.clone();
        inner.entries[0] = fake_entry("srv", replacement_transport.clone());
        inner.entries[0].tools[0].description = "changed contract".into();
        inner.generation += 1;
        publish(
            &inner,
            session.handle.index.as_ref(),
            session.handle.snapshot.as_ref(),
        );

        let current = session.bind_tool(TOOL_NAME).unwrap();
        assert!(Arc::ptr_eq(&binding.tool.transport, &original_transport));
        assert!(Arc::ptr_eq(&current.tool.transport, &replacement_transport));
        assert_ne!(reviewed_subject, *current.subject());
        assert_ne!(binding.generation(), current.generation());
    }

    #[test]
    fn parse_entries_sorts_servers_by_name() {
        let config = make_config(vec![
            ("zeta", stdio_raw(&["z"])),
            ("alpha", stdio_raw(&["a"])),
            ("mid", stdio_raw(&["m"])),
        ]);
        let inner = parse_entries(config, None);
        let names: Vec<&str> = inner.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn startup_trust_follows_project_global_and_runtime_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let mut config = make_config(vec![
            ("global", stdio_raw(&[MISSING_PROGRAM])),
            ("project", stdio_raw(&[MISSING_PROGRAM])),
            ("project-risky-http", http_raw("http://example.com/mcp")),
            ("project-safe-http", http_raw("https://8.8.8.8/mcp")),
        ]);
        config.project_root = Some(project.canonicalize().unwrap());
        config
            .sources
            .insert("project".into(), McpConfigSource::Project);
        config
            .sources
            .insert("project-risky-http".into(), McpConfigSource::Project);
        config
            .sources
            .insert("project-safe-http".into(), McpConfigSource::Project);
        merge_runtime_servers(
            &mut config,
            vec![(
                "runtime".into(),
                RawTransport::Stdio(RawStdioFields {
                    command: vec![MISSING_PROGRAM.into()],
                    environment: HashMap::new(),
                }),
            )],
        );

        let inner = parse_entries(config, None);
        let status = |name: &str| {
            &inner
                .entries
                .iter()
                .find(|entry| entry.name == name)
                .unwrap()
                .status
        };
        assert_eq!(*status("global"), McpServerStatus::Connecting);
        assert_eq!(*status("project"), McpServerStatus::AwaitingTrust);
        assert_eq!(
            *status("project-risky-http"),
            McpServerStatus::AwaitingTrust
        );
        assert_eq!(*status("project-safe-http"), McpServerStatus::Connecting);
        assert_eq!(*status("runtime"), McpServerStatus::Connecting);
        assert_eq!(
            inner
                .entries
                .iter()
                .find(|entry| entry.name == "runtime")
                .unwrap()
                .review
                .config_source,
            McpConfigSource::Runtime
        );
    }

    #[test]
    fn project_stdio_is_not_spawn_eligible_before_trust() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let inner = parse_entries(
            make_project_config(&project, vec![("project", stdio_raw(&[MISSING_PROGRAM]))]),
            None,
        );
        let (tx, _rx) = flume::unbounded();

        assert_eq!(inner.entries[0].status, McpServerStatus::AwaitingTrust);
        assert!(spawn_connects(&inner, STARTUP_EPOCH, &tx).is_empty());
    }

    #[test_case(McpConfigSource::Global; "global")]
    #[test_case(McpConfigSource::Runtime; "runtime")]
    fn remote_local_stdio_is_parked_until_explicit_trust(source: McpConfigSource) {
        smol::block_on(async {
            let mut config = make_config(vec![("remote-local", stdio_raw(&[MISSING_PROGRAM]))]);
            config.local_execution = LocalExecutionPolicy::RemoteLocal;
            config.sources.insert("remote-local".into(), source);
            assert_eq!(
                config.preliminary_infos(&[])[0].status,
                McpServerStatus::AwaitingTrust
            );
            let mut inner = parse_entries(config, None);
            let entry = &inner.entries[0];
            assert_eq!(
                entry.config.as_ref().unwrap().local_execution,
                LocalExecutionPolicy::RemoteLocal
            );
            assert_eq!(entry.status, McpServerStatus::AwaitingTrust);
            let (tx, _rx) = flume::unbounded();
            assert!(spawn_connects(&inner, STARTUP_EPOCH, &tx).is_empty());
            refresh_server(&mut inner, "remote-local").await.unwrap();
            assert_eq!(inner.entries[0].status, McpServerStatus::AwaitingTrust);
            handle_trust(&mut inner, "remote-local", false).await;
            assert!(entry_has_startup_trust(&inner.entries[0], None));
            assert!(matches!(
                inner.entries[0].status,
                McpServerStatus::Failed(_)
            ));
        });
    }

    #[test]
    fn persisted_trust_requires_the_exact_config_digest_without_storing_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let state_dir = StateDir::from_path(tmp.path().join("state"));
        let mut raw = stdio_raw(&["runner"]);
        let RawTransport::Stdio(stdio) = &mut raw.transport else {
            unreachable!();
        };
        stdio
            .environment
            .insert("TOKEN".into(), "raw-secret-value".into());
        let digest = security_digest(&raw);
        trust_project(&state_dir, &project, "project", &digest).unwrap();

        let trusted = parse_entries(
            make_project_config(&project, vec![("project", raw.clone())]),
            Some(state_dir.clone()),
        );
        assert_eq!(trusted.entries[0].status, McpServerStatus::Connecting);

        let RawTransport::Stdio(stdio) = &mut raw.transport else {
            unreachable!();
        };
        stdio.command.push("changed".into());
        let drifted = parse_entries(
            make_project_config(&project, vec![("project", raw)]),
            Some(state_dir.clone()),
        );
        assert_eq!(drifted.entries[0].status, McpServerStatus::AwaitingTrust);
        let persisted = SessionDatabase::open_state(&state_dir)
            .unwrap()
            .state_get::<HashMap<String, String>>(&project_scope(&project), "mcp.trust")
            .unwrap()
            .unwrap();
        let persisted = serde_json::to_string(&persisted).unwrap();
        assert!(!persisted.contains("raw-secret-value"));
    }

    #[test]
    fn persisted_http_trust_cannot_bypass_failed_dns_pinning() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let state_dir = StateDir::from_path(tmp.path().join("state"));
        let raw = http_raw("https://[invalid");
        let digest = security_digest(&raw);
        trust_project(&state_dir, &project, "project", &digest).unwrap();

        let inner = parse_entries(
            make_project_config(&project, vec![("project", raw)]),
            Some(state_dir),
        );

        assert_eq!(inner.entries[0].status, McpServerStatus::AwaitingTrust);
        let (tx, _rx) = flume::unbounded();
        assert!(spawn_connects(&inner, STARTUP_EPOCH, &tx).is_empty());
    }

    #[test]
    fn enabling_trusted_http_server_still_requires_a_pinned_address() {
        smol::block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let state_dir = StateDir::from_path(tmp.path().join("state"));
            let mut raw = http_raw("https://[invalid");
            raw.enabled = false;
            let digest = security_digest(&raw);
            trust_project(&state_dir, &project, "project", &digest).unwrap();
            let mut inner = parse_entries(
                make_project_config(&project, vec![("project", raw)]),
                Some(state_dir),
            );

            handle_toggle(&mut inner, "project", true).await;

            assert_eq!(inner.entries[0].status, McpServerStatus::AwaitingTrust);
            assert!(inner.entries[0].transport.is_none());
        });
    }

    #[test]
    fn awaiting_trust_is_ready_and_parked() {
        smol::block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let config =
                make_project_config(&project, vec![("project", stdio_raw(&[MISSING_PROGRAM]))]);
            let state_dir = StateDir::from_path(tmp.path().join("state"));
            let handle = start_with_config_and_state(config, Some(state_dir)).unwrap();

            handle.ready().await;

            let snapshot = handle.reader().load_full();
            assert_eq!(snapshot.infos[0].status, McpServerStatus::AwaitingTrust);
            assert!(snapshot.pids.is_empty());
            handle.shutdown().await;
        });
    }

    #[test]
    fn trust_once_allows_start_and_refresh_actions_recheck_trust() {
        smol::block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let mut inner = parse_entries(
                make_project_config(&project, vec![("project", stdio_raw(&[MISSING_PROGRAM]))]),
                None,
            );

            handle_reconnect(&mut inner, "project").await;
            assert_eq!(inner.entries[0].status, McpServerStatus::AwaitingTrust);

            let mut disabled = stdio_raw(&[MISSING_PROGRAM]);
            disabled.enabled = false;
            let mut toggled = parse_entries(
                make_project_config(&project, vec![("project", disabled)]),
                None,
            );
            handle_toggle(&mut toggled, "project", true).await;
            assert_eq!(toggled.entries[0].status, McpServerStatus::AwaitingTrust);

            handle_trust(&mut inner, "project", false).await;
            assert!(matches!(
                inner.entries[0].status,
                McpServerStatus::Failed(_)
            ));
        });
    }

    #[test]
    fn reject_revokes_project_trust_and_disables_server() {
        smol::block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let state_dir = StateDir::from_path(tmp.path().join("state"));
            let raw = stdio_raw(&[MISSING_PROGRAM]);
            let digest = security_digest(&raw);
            trust_project(&state_dir, &project, "project", &digest).unwrap();
            let mut inner = parse_entries(
                make_project_config(&project, vec![("project", raw)]),
                Some(state_dir.clone()),
            );

            handle_reject(&mut inner, "project").await;

            assert_eq!(inner.entries[0].status, McpServerStatus::Disabled);
            assert!(!is_project_trusted(&state_dir, &project, "project", &digest).unwrap());
        });
    }

    #[test]
    fn trust_project_action_persists_exact_config() {
        smol::block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let state_dir = StateDir::from_path(tmp.path().join("state"));
            let raw = stdio_raw(&[MISSING_PROGRAM]);
            let digest = security_digest(&raw);
            let mut inner = parse_entries(
                make_project_config(&project, vec![("project", raw)]),
                Some(state_dir.clone()),
            );

            handle_trust(&mut inner, "project", true).await;

            assert!(is_project_trusted(&state_dir, &project, "project", &digest).unwrap());
            assert!(matches!(
                inner.entries[0].status,
                McpServerStatus::Failed(_)
            ));
        });
    }

    fn listed(session: &McpSession) -> Vec<String> {
        session
            .servers()
            .into_iter()
            .flat_map(|server| {
                let name = server.name;
                server
                    .tools
                    .into_iter()
                    .map(move |tool| format!("{name}{SEPARATOR}{}", tool.name))
            })
            .collect()
    }

    /// A disabled tool must not surface as a name code mode offers either: that is the whole
    /// point of turning it off.
    #[test_case(&["srv.tool".to_owned()] ; "qualified_name")]
    #[test_case(&["srv.*".to_owned()] ; "server_wildcard")]
    fn a_disabled_tool_stays_out_of_what_code_mode_offers(disabled: &[String]) {
        let (_inner, session) = setup(vec![
            fake_entry("srv", FakeTransport::new()),
            fake_entry(OTHER_SERVER, FakeTransport::new()),
        ]);
        assert_eq!(
            listed(&session.with_disabled_tools(disabled)),
            [format!("{OTHER_SERVER}{SEPARATOR}tool")]
        );
    }

    fn tool_def(server: &str, raw: &str, description: &str, schema: Value) -> McpToolDef {
        McpToolDef {
            qualified_name: intern(format!("{server}{SEPARATOR}{raw}")),
            raw_name: raw.into(),
            description: description.into(),
            input_schema: schema,
            meta: Arc::default(),
        }
    }

    #[test_case("eager", false; "explicit_eager")]
    #[test_case("lazy", true; "explicit_lazy")]
    fn a_lazy_profile_keeps_a_tool_searchable_but_unlisted(exposure: &str, search_only: bool) {
        let profile =
            Arc::new(serde_json::from_value(json!({"overrides":{"srv.*":exposure}})).unwrap());
        let (_inner, session) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let session = session.with_actor_policy(profile);
        let [status] = session.tool_inventory().try_into().unwrap();
        assert_eq!(
            (status.disabled, status.search_only, status.reason),
            (false, search_only, Some(PROFILE_LOADING))
        );
        assert_eq!(session.servers()[0].tools[0].search_only, search_only);
    }

    #[test_case(AgentMode::Build, false; "build")]
    #[test_case(AgentMode::ReadOnly, true; "read_only")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), true; "local_plan")]
    #[test_case(AgentMode::RemotePlan(PlanRef::new(PLAN_REFERENCE).unwrap()), true; "remote_plan")]
    fn a_prepared_mode_ceiling_matches_the_live_one(mode: AgentMode, disabled: bool) {
        let session = stub_session(&[(REPLACEMENT_TOOL_NAME, REPLACEMENT_DESCRIPTION)]);
        let prepared = session
            .clone()
            .with_profile_policy(Arc::default(), ToolFilter::All.for_mcp_mode(&mode));
        let live = session.narrowed(&ToolFilter::All.for_mcp_mode(&mode));
        for candidate in [&prepared, &live] {
            let [status] = candidate.tool_inventory().try_into().unwrap();
            assert_eq!(
                (status.disabled, status.reason),
                (disabled, disabled.then_some(MODE_DISABLED))
            );
            assert_eq!(candidate.is_disabled(REPLACEMENT_TOOL_NAME), disabled);
        }
    }

    #[test]
    fn a_profile_disables_tools_published_after_it() {
        let (mut inner, session) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let profile =
            Arc::new(serde_json::from_value(json!({"overrides":{"srv.*":"disabled"}})).unwrap());
        let session = session.with_actor_policy(profile);
        inner.entries[0].tools = vec![tool_def(
            "srv",
            "replacement",
            REPLACEMENT_DESCRIPTION,
            json!({}),
        )];
        publish(&inner, &session.handle.index, &session.handle.snapshot);
        assert!(listed(&session).is_empty());
        let [status] = session.tool_inventory().try_into().unwrap();
        assert_eq!(
            (status.disabled, status.reason),
            (true, Some(PROFILE_DISABLED))
        );
    }

    #[test]
    fn generic_mcp_ceiling_cannot_be_replaced_by_worker_profile() {
        let profile = Arc::new(serde_json::from_value(json!({"default":"disabled"})).unwrap());
        let session = stub_session(&[(REPLACEMENT_TOOL_NAME, REPLACEMENT_DESCRIPTION)])
            .with_actor_policy(profile)
            .freeze_actor_ceiling()
            .with_actor_policy(Arc::default());
        assert!(session.is_disabled(REPLACEMENT_TOOL_NAME));
        assert!(session.tool_inventory()[0].disabled);
    }

    /// `ready` carries the correctness of connecting in the background: a prompt
    /// typed during startup must not ship before the servers settle.
    #[test]
    fn ready_settles_every_server_status() {
        smol::block_on(async {
            assert!(start_with_config(McpConfig::default()).is_none());

            let mut disabled = stdio_raw(&["unused-disabled-cmd"]);
            disabled.enabled = false;
            let config = make_config(vec![
                ("disabled-srv", disabled),
                ("unparseable-srv", stdio_raw(&[])),
                ("unspawnable-srv", stdio_raw(&[MISSING_PROGRAM])),
            ]);
            let handle = start_with_config(config).unwrap();
            handle.ready().await;

            let infos = handle.reader().load().infos.clone();
            let status = |name: &str| {
                &infos
                    .iter()
                    .find(|i| i.name == name)
                    .unwrap_or_else(|| panic!("{name} must be published"))
                    .status
            };
            let failed = |name: &str| matches!(status(name), McpServerStatus::Failed(_));
            assert!(failed("unparseable-srv"));
            assert!(failed("unspawnable-srv"));
            assert_eq!(*status("disabled-srv"), McpServerStatus::Disabled);
        });
    }

    /// `sleep` spawns fine and never answers `initialize`, so its connect only
    /// ends on the request timeout, far past the shutdown one. Shutdown has to
    /// preempt it, or quitting during startup hangs.
    #[cfg(unix)]
    #[test]
    fn shutdown_preempts_an_in_flight_connect() {
        const BLOCKED: &str = "shutdown must not wait for an in-flight connect";
        smol::block_on(async {
            let config = make_config(vec![("slow-srv", stdio_raw(&["sleep", "60"]))]);
            let handle = start_with_config(config).unwrap();
            let started = Instant::now();
            handle.shutdown().await;
            assert!(started.elapsed() < MCP_SHUTDOWN_TIMEOUT, "{BLOCKED}");
        });
    }

    /// If a refresh fails, the entry must end up empty. A zombie tool left behind would be
    /// handed to the model on the next turn and then try to call into a dead transport.
    #[test]
    fn failed_refresh_clears_entry() {
        smol::block_on(async {
            let t = FakeTransport::new();
            let (mut inner, _) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);
            inner.entries[0].config = Some(bad_stdio_config("srv"));

            assert!(refresh_server(&mut inner, "srv").await.is_err());

            let entry = &inner.entries[0];
            assert_eq!(t.shutdowns(), 1);
            assert!(entry.tools.is_empty());
            assert!(entry.prompts.is_empty());
            assert!(entry.transport.is_none());
            assert!(matches!(entry.status, McpServerStatus::Failed(_)));
        });
    }

    #[test]
    fn disable_purges_entry_and_published_view() {
        smol::block_on(async {
            let t = FakeTransport::new();
            let (mut inner, handle) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);

            assert!(handle.has_tool(TOOL_NAME));
            assert!(handle.has_enabled_server());

            handle_toggle(&mut inner, "srv", false).await;
            publish(&inner, &handle.index, &handle.snapshot);

            let entry = &inner.entries[0];
            assert_eq!(t.shutdowns(), 1);
            assert!(entry.tools.is_empty());
            assert!(entry.transport.is_none());
            assert_eq!(entry.status, McpServerStatus::Disabled);
            assert!(!handle.has_tool(TOOL_NAME));
            assert!(!handle.has_enabled_server());
            assert!(handle.servers().is_empty());
        });
    }

    /// Regression: the lock-free refactor fixed a case where `call_tool` held the inner read
    /// lock across the transport await, so any in-flight call blocked every publish behind it.
    /// The rendezvous here stays deterministic: the call signals on `call_entered`, the test
    /// waits for that signal, then calls `publish` while the call is still parked on `call_gate`.
    #[test]
    fn slow_tool_call_does_not_block_publish() {
        smol::block_on(async {
            let t = FakeTransport::new();
            let (mut inner, handle) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);

            let held = t.call_gate.lock().await;
            let entered = t.call_entered_rx.clone();
            let call_handle = {
                let handle = handle.clone();
                smol::spawn(async move {
                    handle
                        .call_tool(TOOL_NAME, &json!({}), &CallScope::none())
                        .await
                        .unwrap()
                })
            };

            entered.recv_async().await.unwrap();
            inner.generation += 1;
            publish(&inner, &handle.index, &handle.snapshot);
            assert_eq!(handle.snapshot.load().generation, 1);

            drop(held);
            call_handle.await;
        });
    }

    #[test]
    fn shutdown_command_drains_and_acks() {
        smol::block_on(async {
            let (t1, t2) = (FakeTransport::new(), FakeTransport::new());
            let inner = McpManagerInner {
                entries: vec![
                    fake_entry("a", Arc::clone(&t1) as _),
                    fake_entry("b", Arc::clone(&t2) as _),
                ],
                state_dir: None,
                generation: 0,
                local_execution: LocalExecutionPolicy::Embedded,
            };
            let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
            let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
            let (cmd_tx, cmd_rx) = flume::unbounded();
            let loop_task = smol::spawn(run(
                inner,
                Arc::clone(&index),
                Arc::clone(&snapshot),
                cmd_rx,
                flume::bounded(0).0,
            ));

            let (ack_tx, ack_rx) = flume::bounded(1);
            cmd_tx.send(McpCommand::Shutdown { ack: ack_tx }).unwrap();
            ack_rx.recv_async().await.unwrap();
            loop_task.await;

            assert_eq!(t1.shutdowns(), 1);
            assert_eq!(t2.shutdowns(), 1);
            assert!(snapshot.load().infos.iter().all(|i| i.tool_count == 0));
        });
    }

    fn stdio_with_secret(secret: &str) -> RawServerConfig {
        let mut raw = stdio_raw(&[MISSING_PROGRAM]);
        let RawTransport::Stdio(stdio) = &mut raw.transport else {
            unreachable!();
        };
        stdio.environment.insert(SECRET_KEY.into(), secret.into());
        raw
    }

    fn add_argument(raw: &mut RawServerConfig) {
        if let RawTransport::Stdio(stdio) = &mut raw.transport {
            stdio.command.push(CHANGED_ARGUMENT.into());
        }
    }

    fn change_timeout(raw: &mut RawServerConfig) {
        raw.timeout = CHANGED_TIMEOUT_MS;
    }

    /// Stands in for a connect that succeeded, so a test can watch the
    /// connection survive or die without spawning a server.
    fn connect_fake(inner: &mut McpManagerInner, server: &str) -> Arc<FakeTransport> {
        let transport = FakeTransport::new();
        inner
            .entries
            .iter_mut()
            .find(|entry| entry.name == server)
            .unwrap()
            .populate(StartResult {
                transport: transport.clone(),
                discovery: ServerDiscovery::default(),
                tool_infos: vec![protocol::ToolInfo {
                    name: FAKE_TOOL.into(),
                    description: String::new(),
                    input_schema: json!({}),
                    ..protocol::ToolInfo::default()
                }],
                prompt_infos: Vec::new(),
                resources: ResourceLists::default(),
            });
        transport
    }

    fn entry<'a>(inner: &'a McpManagerInner, server: &str) -> &'a ServerEntry {
        inner
            .entries
            .iter()
            .find(|entry| entry.name == server)
            .unwrap()
    }

    /// One reload step as `run` takes it. The returned connects must outlive
    /// any receive, since dropping one cancels its start.
    async fn reload(
        inner: &mut McpManagerInner,
        config: McpConfig,
    ) -> (Connects, flume::Receiver<Connect>) {
        let (tx, rx) = flume::unbounded();
        let mut connects = Connects::new();
        handle_reload(inner, &mut connects, &tx, config).await;
        (connects, rx)
    }

    #[test]
    fn reload_with_an_unchanged_config_keeps_the_connection() {
        smol::block_on(async {
            let config = || make_config(vec![(RELOAD_SERVER, stdio_raw(&[MISSING_PROGRAM]))]);
            let mut inner = parse_entries(config(), None);
            let transport = connect_fake(&mut inner, RELOAD_SERVER);

            let (connects, rx) = reload(&mut inner, config()).await;

            assert!(connects.is_empty() && rx.is_empty());
            assert_eq!(transport.shutdowns(), 0);
            assert_eq!(
                entry(&inner, RELOAD_SERVER).status,
                McpServerStatus::Running
            );
            assert!(published(&inner).has_tool(TOOL_NAME));
        });
    }

    #[test_case(false; "removed")]
    #[test_case(true; "disabled")]
    fn reload_stops_a_server_and_drops_its_tools(still_listed: bool) {
        smol::block_on(async {
            let mut inner = parse_entries(
                make_config(vec![(RELOAD_SERVER, stdio_raw(&[MISSING_PROGRAM]))]),
                None,
            );
            let transport = connect_fake(&mut inner, RELOAD_SERVER);
            let mut disabled = stdio_raw(&[MISSING_PROGRAM]);
            disabled.enabled = false;
            let servers = if still_listed {
                vec![(RELOAD_SERVER, disabled)]
            } else {
                Vec::new()
            };

            let (connects, _rx) = reload(&mut inner, make_config(servers)).await;

            let handle = published(&inner);
            let statuses: Vec<McpServerStatus> = handle
                .reader()
                .load()
                .infos
                .iter()
                .map(|info| info.status.clone())
                .collect();
            assert!(connects.is_empty());
            assert_eq!(transport.shutdowns(), 1);
            assert!(!handle.has_tool(TOOL_NAME));
            assert_eq!(
                statuses,
                still_listed
                    .then_some(McpServerStatus::Disabled)
                    .into_iter()
                    .collect::<Vec<_>>()
            );
        });
    }

    #[test_case(McpConfigSource::Global, true; "global_starts")]
    #[test_case(McpConfigSource::Project, false; "project_awaits_trust")]
    fn reload_starts_an_added_server_under_startup_rules(source: McpConfigSource, starts: bool) {
        smol::block_on(async {
            let running = (RELOAD_SERVER, stdio_raw(&[MISSING_PROGRAM]));
            let mut inner = parse_entries(make_config(vec![running.clone()]), None);
            let transport = connect_fake(&mut inner, RELOAD_SERVER);
            let mut config =
                make_config(vec![running, (OTHER_SERVER, stdio_raw(&[MISSING_PROGRAM]))]);
            config.sources.insert(OTHER_SERVER.into(), source);

            let (connects, rx) = reload(&mut inner, config).await;

            assert_eq!(transport.shutdowns(), 0);
            assert_eq!(connects.contains_key(OTHER_SERVER), starts);
            let added = entry(&inner, OTHER_SERVER);
            if starts {
                assert_eq!(added.status, McpServerStatus::Connecting);
                let connect = rx.recv_async().await.unwrap();
                assert_eq!(
                    (connect.server.as_str(), connect.epoch),
                    (OTHER_SERVER, added.epoch)
                );
            } else {
                assert_eq!(added.status, McpServerStatus::AwaitingTrust);
                assert!(rx.is_empty());
            }
        });
    }

    #[test_case(add_argument; "command")]
    #[test_case(change_timeout; "timeout")]
    fn reload_restarts_a_changed_server(change: fn(&mut RawServerConfig)) {
        smol::block_on(async {
            let mut raw = stdio_raw(&[MISSING_PROGRAM]);
            let mut inner = parse_entries(make_config(vec![(RELOAD_SERVER, raw.clone())]), None);
            let transport = connect_fake(&mut inner, RELOAD_SERVER);
            change(&mut raw);

            let (connects, rx) = reload(&mut inner, make_config(vec![(RELOAD_SERVER, raw)])).await;

            assert_eq!(transport.shutdowns(), 1);
            assert!(!published(&inner).has_tool(TOOL_NAME));
            assert_eq!(
                entry(&inner, RELOAD_SERVER).status,
                McpServerStatus::Connecting
            );
            assert!(connects.contains_key(RELOAD_SERVER));
            assert_eq!(rx.recv_async().await.unwrap().server, RELOAD_SERVER);
        });
    }

    /// Trust covers the reviewed definition: a rotated secret parks the server
    /// exactly as a new untrusted one, a new timeout alone restarts it.
    #[test_case(true, true; "project_trust_rotated_secret")]
    #[test_case(false, true; "session_trust_rotated_secret")]
    #[test_case(true, false; "project_trust_new_timeout")]
    #[test_case(false, false; "session_trust_new_timeout")]
    fn reload_rechecks_trust_against_the_reviewed_definition(persist: bool, rotate: bool) {
        smol::block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = StateDir::from_path(tmp.path().join("state"));
            let mut inner = parse_entries(
                make_project_config(
                    &project,
                    vec![(RELOAD_SERVER, stdio_with_secret(FIRST_SECRET))],
                ),
                Some(state_dir),
            );
            handle_trust(&mut inner, RELOAD_SERVER, persist).await;
            let transport = connect_fake(&mut inner, RELOAD_SERVER);
            let mut changed = stdio_with_secret(if rotate { ROTATED_SECRET } else { FIRST_SECRET });
            if !rotate {
                change_timeout(&mut changed);
            }

            let (connects, rx) = reload(
                &mut inner,
                make_project_config(&project, vec![(RELOAD_SERVER, changed)]),
            )
            .await;

            let handle = published(&inner);
            let infos = handle.reader().load_full().infos.clone();
            let rendered = format!("{infos:?}");
            assert_eq!(transport.shutdowns(), 1);
            assert!(!handle.has_tool(TOOL_NAME));
            assert_eq!(connects.contains_key(RELOAD_SERVER), !rotate);
            assert_eq!(infos[0].review.environment_names, [SECRET_KEY]);
            assert!(!rendered.contains(FIRST_SECRET) && !rendered.contains(ROTATED_SECRET));
            if rotate {
                assert_eq!(infos[0].status, McpServerStatus::AwaitingTrust);
                assert!(rx.is_empty());
            } else {
                assert_eq!(infos[0].status, McpServerStatus::Connecting);
                assert_eq!(rx.recv_async().await.unwrap().server, RELOAD_SERVER);
            }
        });
    }

    #[test]
    fn reload_parks_a_remote_local_server_when_its_env_file_value_rotates() {
        smol::block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let global = tmp.path().join("global");
            std::fs::create_dir(&global).unwrap();
            std::fs::write(
                global.join(MCP_FILE),
                format!(
                    "[mcp.{RELOAD_SERVER}]\ncommand = ['{MISSING_PROGRAM}']\nenvironment = {{ {SECRET_KEY} = '{{env:{SECRET_VARIABLE}}}' }}\n"
                ),
            )
            .unwrap();
            let write_secret = |secret: &str| {
                std::fs::write(
                    global.join(ENV_FILE),
                    format!("{SECRET_VARIABLE}={secret}\n"),
                )
                .unwrap();
            };
            let load = || {
                config::complete(config::load_global_config_from(tmp.path(), Some(&global)))
                    .unwrap()
            };
            write_secret(FIRST_SECRET);
            let mut inner = parse_entries(load(), None);
            handle_trust(&mut inner, RELOAD_SERVER, false).await;
            let transport = connect_fake(&mut inner, RELOAD_SERVER);

            let (connects, _rx) = reload(&mut inner, load()).await;
            assert!(connects.is_empty());
            assert_eq!(transport.shutdowns(), 0);

            write_secret(ROTATED_SECRET);
            let (connects, _rx) = reload(&mut inner, load()).await;
            assert!(connects.is_empty());
            assert_eq!(transport.shutdowns(), 1);
            assert_eq!(
                entry(&inner, RELOAD_SERVER).status,
                McpServerStatus::AwaitingTrust
            );
        });
    }

    #[test_case(true; "toggled_off")]
    #[test_case(false; "toggled_on")]
    fn reload_of_the_file_a_toggle_wrote_is_a_no_op(initially_enabled: bool) {
        smol::block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let global = tmp.path().join("global");
            std::fs::create_dir(&global).unwrap();
            std::fs::write(
                global.join(MCP_FILE),
                format!(
                    "[mcp.{RELOAD_SERVER}]\ncommand = ['{MISSING_PROGRAM}']\nenabled = {initially_enabled}\n"
                ),
            )
            .unwrap();
            let load =
                || config::complete(config::load_config_from(tmp.path(), Some(&global))).unwrap();
            let mut inner = parse_entries(load(), None);
            let transport = initially_enabled.then(|| connect_fake(&mut inner, RELOAD_SERVER));
            // The toggle persists on a detached task; writing here instead
            // leaves the file whole before it is read back.
            let origin = std::mem::take(&mut inner.entries[0].origin);
            handle_toggle(&mut inner, RELOAD_SERVER, !initially_enabled).await;
            config::persist_enabled(&origin, RELOAD_SERVER, !initially_enabled).unwrap();
            let status = entry(&inner, RELOAD_SERVER).status.clone();
            let shutdowns = transport.as_ref().map(|transport| transport.shutdowns());

            let (connects, rx) = reload(&mut inner, load()).await;

            assert!(connects.is_empty() && rx.is_empty());
            assert_eq!(entry(&inner, RELOAD_SERVER).status, status);
            assert_eq!(
                transport.as_ref().map(|transport| transport.shutdowns()),
                shutdowns
            );
        });
    }

    #[test_case(false; "unclaimed")]
    #[test_case(true; "claimed_by_a_file")]
    fn reload_keeps_runtime_servers_unless_a_file_claims_the_name(claimed: bool) {
        smol::block_on(async {
            let mut config = make_config(Vec::new());
            merge_runtime_servers(
                &mut config,
                vec![(
                    RELOAD_SERVER.into(),
                    stdio_raw(&[MISSING_PROGRAM]).transport,
                )],
            );
            let mut inner = parse_entries(config, None);
            let transport = connect_fake(&mut inner, RELOAD_SERVER);
            let servers = if claimed {
                vec![(RELOAD_SERVER, stdio_raw(&[MISSING_PROGRAM]))]
            } else {
                Vec::new()
            };

            let (_connects, _rx) = reload(&mut inner, make_config(servers)).await;

            assert_eq!(transport.shutdowns(), usize::from(claimed));
            assert_eq!(
                entry(&inner, RELOAD_SERVER).source,
                if claimed {
                    McpConfigSource::Global
                } else {
                    McpConfigSource::Runtime
                }
            );
        });
    }

    #[test]
    fn reload_under_another_execution_policy_is_ignored() {
        smol::block_on(async {
            let mut inner = parse_entries(
                make_config(vec![(RELOAD_SERVER, stdio_raw(&[MISSING_PROGRAM]))]),
                None,
            );
            let transport = connect_fake(&mut inner, RELOAD_SERVER);
            let mut config = make_config(Vec::new());
            config.local_execution = LocalExecutionPolicy::RemoteLocal;

            let (connects, _rx) = reload(&mut inner, config).await;

            assert!(connects.is_empty());
            assert_eq!(transport.shutdowns(), 0);
            assert!(published(&inner).has_tool(TOOL_NAME));
        });
    }

    /// Commands run in order, so the shutdown's ack proves the reload applied.
    #[test]
    fn reload_command_applies_before_a_later_shutdown() {
        smol::block_on(async {
            let mut inner = parse_entries(
                make_config(vec![
                    (RELOAD_SERVER, stdio_raw(&[MISSING_PROGRAM])),
                    (OTHER_SERVER, stdio_raw(&[MISSING_PROGRAM])),
                ]),
                None,
            );
            let kept = connect_fake(&mut inner, RELOAD_SERVER);
            let removed = connect_fake(&mut inner, OTHER_SERVER);
            let handle = published(&inner);
            let (cmd_tx, cmd_rx) = flume::unbounded();
            let handle = McpHandle { cmd_tx, ..handle };
            let loop_task = smol::spawn(run(
                inner,
                Arc::clone(&handle.index),
                Arc::clone(&handle.snapshot),
                cmd_rx,
                flume::bounded(0).0,
            ));

            handle.reload(make_config(vec![(
                RELOAD_SERVER,
                stdio_raw(&[MISSING_PROGRAM]),
            )]));
            handle.shutdown().await;
            loop_task.await;

            let names: Vec<String> = handle
                .reader()
                .load()
                .infos
                .iter()
                .map(|info| info.name.clone())
                .collect();
            assert_eq!(names, [RELOAD_SERVER]);
            assert_eq!(removed.shutdowns(), 1);
            assert_eq!(kept.shutdowns(), 1);
        });
    }

    #[test]
    fn is_valid_tool_name_enforces_wire_format() {
        use config::is_valid_tool_name;
        // Valid: alphanumeric, underscore, hyphen, 1-64 chars
        assert!(is_valid_tool_name("search"));
        assert!(is_valid_tool_name("web_search"));
        assert!(is_valid_tool_name("my-tool"));
        assert!(is_valid_tool_name(&"a".repeat(64)));
        // Invalid: empty, dots, special chars, too long
        assert!(!is_valid_tool_name(""));
        assert!(!is_valid_tool_name("web.search"));
        assert!(!is_valid_tool_name("admin.delete"));
        assert!(!is_valid_tool_name("tool!"));
        assert!(!is_valid_tool_name("*"));
        assert!(!is_valid_tool_name(&"a".repeat(65)));
    }

    /// Serves `tools/list` from `tools`, which a test swaps to stand for the
    /// server changing its list, and reads what the server sends unasked.
    struct ListingTransport {
        name: Arc<str>,
        inbound: Inbound,
        tools: Mutex<Vec<&'static str>>,
    }

    impl ListingTransport {
        fn new(tools: &[&'static str]) -> Arc<Self> {
            let name: Arc<str> = Arc::from(RELOAD_SERVER);
            Arc::new(Self {
                inbound: Inbound::new(Arc::clone(&name)),
                name,
                tools: Mutex::new(tools.to_vec()),
            })
        }
    }

    impl McpTransport for ListingTransport {
        fn send_request<'a>(
            &'a self,
            _method: &'a str,
            _params: Option<Value>,
            _scope: &'a CallScope,
        ) -> transport::BoxFuture<'a, Result<Value, McpError>> {
            let tools: Vec<Value> = self
                .tools
                .lock()
                .unwrap()
                .iter()
                .map(|name| json!({"name": name}))
                .collect();
            Box::pin(async move { Ok(json!({ "tools": tools })) })
        }
        fn send_notification<'a>(
            &'a self,
            _method: &'a str,
            _params: Option<Value>,
        ) -> transport::BoxFuture<'a, Result<(), McpError>> {
            Box::pin(async { Ok(()) })
        }
        fn shutdown<'a>(&'a self) -> transport::BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn server_name(&self) -> &Arc<str> {
            &self.name
        }
        fn transport_kind(&self) -> &'static str {
            RELOAD_SERVER
        }
        fn inbound(&self) -> Option<&Inbound> {
            Some(&self.inbound)
        }
    }

    fn qualified(tool: &str) -> String {
        format!("{RELOAD_SERVER}{SEPARATOR}{tool}")
    }

    #[test]
    fn a_tool_list_change_is_relisted_and_republished() {
        smol::block_on(async {
            let transport = ListingTransport::new(&[FAKE_TOOL]);
            let (mut inner, handle) =
                setup(vec![fake_entry(RELOAD_SERVER, Arc::clone(&transport) as _)]);
            let (relisted_tx, relisted_rx) = flume::unbounded();
            inner.entries[0].watch_lists(&relisted_tx);

            *transport.tools.lock().unwrap() = vec![FAKE_TOOL, ADDED_TOOL];
            for _ in 0..LIST_CHANGE_BURST {
                transport.inbound.notify(Notification {
                    method: TOOLS_LIST_CHANGED.into(),
                    params: Value::Null,
                });
            }
            let relisted = relisted_rx.recv_async().await.unwrap();
            assert!(relisted_rx.is_empty(), "a burst must cost one listing");
            assert!(apply_relisted(&mut inner, relisted));
            inner.generation += 1;
            publish(&inner, &handle.index, &handle.snapshot);

            assert!(handle.has_tool(&qualified(ADDED_TOOL)));
            assert_eq!(handle.snapshot.load().infos[0].tool_count, 2);
            assert_eq!(handle.snapshot.load().generation, 1);
        });
    }

    #[test]
    fn a_relisting_from_a_replaced_connection_is_dropped() {
        let (mut inner, _handle) = setup(vec![fake_entry(RELOAD_SERVER, FakeTransport::new())]);
        let stale = Relisted {
            server: RELOAD_SERVER.into(),
            connection: ListingTransport::new(&[]),
            tools: Some(Ok(Vec::new())),
            resources: None,
            prompts: None,
        };
        assert!(!apply_relisted(&mut inner, stale));
        assert_eq!(inner.entries[0].tools.len(), 1);
    }

    #[test_case(json!({"$ref": "https://example.com/schema.json"}), false ; "remote_ref_excluded")]
    #[test_case(json!({"properties": {"a": {"anyOf": [{"$ref": "other.json#/a"}]}}}), false ; "nested_relative_ref_excluded")]
    #[test_case(json!({"properties": {"a": {"$ref": "#/$defs/a"}}, "$defs": {"a": {"type": "string"}}}), true ; "local_ref_kept")]
    #[test_case(json!({"properties": {"$ref": {"type": "string"}}}), true ; "property_named_ref_kept")]
    fn a_tool_whose_schema_needs_an_external_ref_is_excluded(schema: Value, kept: bool) {
        let tools = tool_defs(
            RELOAD_SERVER,
            vec![ToolInfo {
                name: FAKE_TOOL.into(),
                input_schema: schema,
                ..ToolInfo::default()
            }],
        );
        assert_eq!(tools.len(), usize::from(kept));
    }

    #[test_case(Some(TOOL_TITLE), None, Some(TOOL_TITLE) ; "own_title")]
    #[test_case(Some(TOOL_TITLE), Some(ANNOTATION_TITLE), Some(TOOL_TITLE) ; "own_title_wins")]
    #[test_case(None, Some(ANNOTATION_TITLE), Some(ANNOTATION_TITLE) ; "annotation_title")]
    #[test_case(None, None, None ; "untitled")]
    fn a_tool_title_falls_back_to_its_annotation(
        title: Option<&str>,
        annotation: Option<&str>,
        expected: Option<&str>,
    ) {
        let meta = McpToolMeta {
            title: title.map(str::to_owned),
            annotations: ToolAnnotations {
                title: annotation.map(str::to_owned),
                ..ToolAnnotations::default()
            },
            output_schema: None,
        };
        assert_eq!(meta.title(), expected);
    }

    #[test]
    fn a_bound_tool_and_its_server_info_carry_what_the_server_said() {
        let (mut inner, _handle) = setup(vec![fake_entry(RELOAD_SERVER, FakeTransport::new())]);
        let discovery = ServerDiscovery {
            protocol_version: NEGOTIATED_PROTOCOL.into(),
            server_info: ServerInfo {
                name: EXECUTOR_SERVER_NAME.into(),
                ..ServerInfo::default()
            },
            instructions: Some(INSTRUCTIONS.into()),
            ..ServerDiscovery::default()
        };
        inner.entries[0].populate(StartResult {
            transport: FakeTransport::new(),
            discovery: discovery.clone(),
            tool_infos: vec![ToolInfo {
                name: FAKE_TOOL.into(),
                title: Some(TOOL_TITLE.into()),
                annotations: ToolAnnotations {
                    read_only_hint: Some(true),
                    ..ToolAnnotations::default()
                },
                ..ToolInfo::default()
            }],
            prompt_infos: Vec::new(),
            resources: ResourceLists::default(),
        });
        let handle = published(&inner);

        let binding = handle.bind_tool(&qualified(FAKE_TOOL)).unwrap();
        assert_eq!(binding.kind(), Some(McpServerKind::Executor));
        assert_eq!(binding.title(), Some(TOOL_TITLE));
        assert!(binding.annotations().read_only());
        let info = &handle.snapshot.load().infos[0];
        assert_eq!(info.kind, Some(McpServerKind::Executor));
        assert_eq!(info.discovery.as_ref(), Some(&discovery));
    }

    const HOME_ASSISTANT_SERVER_NAME: &str = "home-assistant";
    const LOOKALIKE_SERVER_NAME: &str = "home-assistant-ish";
    const LOOKUP_RAW: &str = "lookup";
    const LOOKUP_TOOL: &str = "ha.lookup";
    const EXECUTE_RAW: &str = "execute";
    const RESUME_RAW: &str = "resume";
    const PAUSED_RUN: &str = "exec_1";
    const WAITING_STATUS: &str = "waiting_for_interaction";
    const PAUSED_MESSAGE: &str = "Deploy to prod?";
    const PAUSED_ADDRESS: &str = "deploy.run";
    const PAUSED_NOTE: &str =
        "Executor run waiting for: Deploy to prod? · deploy.run({\"env\":\"prod\"})";
    const UNSEEN_NOTE: &str = "Executor run exec_1: not a run Caudra saw pause";

    fn paused_run() -> Value {
        json!({
            "status": WAITING_STATUS,
            "executionId": PAUSED_RUN,
            "interaction": {"message": PAUSED_MESSAGE, "address": PAUSED_ADDRESS, "args": {"env": "prod"}},
        })
    }

    fn accept_paused_run() -> Value {
        json!({"executionId": PAUSED_RUN, "action": "accept"})
    }

    fn executor_tool(raw: &str) -> String {
        format!("{EXECUTOR_SERVER_NAME}{SEPARATOR}{raw}")
    }

    fn read_only_ceiling() -> ToolFilter {
        ToolFilter::ReadOnly(Box::new(ToolFilter::All))
    }

    /// Only a server Caudra recognises earns trust in its `readOnlyHint`, and
    /// a read-only ceiling keeps exactly those tools, declared and searchable.
    #[test_case(HOME_ASSISTANT_SERVER_NAME, true, true ; "a_recognised_read_only_tool")]
    #[test_case(HOME_ASSISTANT_SERVER_NAME, false, false ; "a_recognised_tool_that_writes")]
    #[test_case(LOOKALIKE_SERVER_NAME, true, false ; "a_stranger_claiming_read_only")]
    fn a_read_only_ceiling_keeps_recognised_read_only_tools(
        server: &str,
        annotated: bool,
        read_only: bool,
    ) {
        let session = recognised_stub_session(server, &[(LOOKUP_TOOL, annotated)], json!({}));
        assert_eq!(
            session.bind_tool(LOOKUP_TOOL).unwrap().read_only(),
            read_only
        );
        assert_eq!(session.is_read_only(LOOKUP_TOOL), read_only);

        let narrowed = session.narrowed(&read_only_ceiling());
        assert_eq!(narrowed.is_disabled(LOOKUP_TOOL), !read_only);
        let [status] = narrowed.tool_inventory().try_into().unwrap();
        assert_eq!(
            (status.disabled, status.reason),
            (!read_only, (!read_only).then_some(MODE_DISABLED))
        );
    }

    /// Recognition and annotations add to a prompt, never to the contract a
    /// grant was filed against, so grants made before them still match.
    #[test]
    fn recognition_leaves_the_contract_digest_alone() {
        let contract = |session: McpSession| match session.bind_tool(LOOKUP_TOOL).unwrap().subject()
        {
            PermissionSubject::Mcp { contract, .. } => contract.clone(),
            subject => panic!("{subject:?}"),
        };
        let expected = canonical_json_sha256(&json!({
            "name": LOOKUP_RAW,
            "description": "",
            "input_schema": {},
        }));
        assert_eq!(contract(stub_session(&[(LOOKUP_TOOL, "")])), expected);
        assert_eq!(
            contract(recognised_stub_session(
                HOME_ASSISTANT_SERVER_NAME,
                &[(LOOKUP_TOOL, true)],
                json!({}),
            )),
            expected
        );
    }

    /// A run an Executor result reports paused is what accepting it later
    /// says it approves.
    #[test]
    fn an_executor_pause_is_remembered_for_its_resume() {
        smol::block_on(async {
            let session = recognised_stub_session(
                EXECUTOR_SERVER_NAME,
                &[
                    (&executor_tool(EXECUTE_RAW), false),
                    (&executor_tool(RESUME_RAW), false),
                ],
                json!({"content": [], "structuredContent": paused_run()}),
            );
            let resume = session.bind_tool(&executor_tool(RESUME_RAW)).unwrap();
            let note = || resume.call_facts(&accept_paused_run()).unwrap().note;
            let unseen = note();
            session
                .bind_tool(&executor_tool(EXECUTE_RAW))
                .unwrap()
                .call(&json!({}), &CallScope::none())
                .await
                .unwrap();
            assert_eq!(
                (unseen.as_deref(), note().as_deref()),
                (Some(UNSEEN_NOTE), Some(PAUSED_NOTE))
            );
        });
    }

    /// A paused run belongs to the connection that reported it.
    #[test]
    fn a_reconnect_forgets_paused_runs() {
        let (mut inner, _handle) = setup(vec![fake_entry(RELOAD_SERVER, FakeTransport::new())]);
        let connect = |inner: &mut McpManagerInner| {
            inner.entries[0].populate(StartResult {
                transport: FakeTransport::new(),
                discovery: ServerDiscovery {
                    server_info: ServerInfo {
                        name: EXECUTOR_SERVER_NAME.into(),
                        ..ServerInfo::default()
                    },
                    ..ServerDiscovery::default()
                },
                tool_infos: vec![ToolInfo {
                    name: RESUME_RAW.into(),
                    ..ToolInfo::default()
                }],
                prompt_infos: Vec::new(),
                resources: ResourceLists::default(),
            });
        };
        let note = |inner: &McpManagerInner| {
            published(inner)
                .bind_tool(&qualified(RESUME_RAW))
                .unwrap()
                .call_facts(&accept_paused_run())
                .unwrap()
                .note
        };
        connect(&mut inner);
        inner.entries[0].paused_runs.record(&paused_run());
        let remembered = note(&inner);
        connect(&mut inner);
        assert_eq!(
            (remembered.as_deref(), note(&inner).as_deref()),
            (Some(PAUSED_NOTE), Some(UNSEEN_NOTE))
        );
    }
}
