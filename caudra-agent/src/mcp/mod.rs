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

pub mod config;
pub mod error;
pub mod http;
pub mod oauth;
pub mod protocol;
pub mod stdio;
pub mod transport;

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use arc_swap::{ArcSwap, Guard};
use caudra_providers::{ContentBlock, Message};
use caudra_storage::StateDir;
use caudra_storage::mcp_trust::{is_project_trusted, revoke_project_trust, trust_project};
use serde_json::{Value, json};
use tracing::{info, warn};

use self::config::{
    LocalExecutionPolicy, McpConfig, McpConfigErrors, McpConfigSource, McpReviewSummary,
    McpServerInfo, McpServerStatus, OauthClientConfig, RawServerConfig, RawTransport, ServerConfig,
    Transport, load_config, load_global_config, parse_server, requires_project_trust,
    resolve_http_addresses, review_summary, risky_ip, security_digest, transport_kind,
};
use self::error::McpError;
use self::http::HttpTransport;
use self::stdio::StdioTransport;
use self::transport::McpTransport;
use crate::permissions::{PermissionSubject, canonical_json_sha256};
use crate::tools::deferral::SearchOutcome;
use crate::tools::schema::sanitize_tool_input_schema;

const SEPARATOR: &str = ".";
const WIRE_SEPARATOR: &str = "__";
pub const UNKNOWN_MCP: &str = "unknown_mcp";
/// Below this many deferrable tools, a search round-trip plus its
/// prompt-cache miss cost more than a handful of upfront definitions.
/// Overridden by `defer_tools` in mcp.toml.
const DEFAULT_DEFER_TOOLS: usize = 10;
/// Loads per search are capped so one broad query can't flood the context.
const MAX_SEARCH_LOADS: usize = 5;
const NAME_HIT_SCORE: usize = 2;
const DESCRIPTION_HIT_SCORE: usize = 1;
/// Overflow names shown to the model so it can re-search by exact name.
const MAX_OVERFLOW_NAMES: usize = 20;
const SEARCH_NO_MATCH: &str = "No deferred MCP tools matched";
const SEARCH_OVERFLOW_PREFIX: &str = "Also matched but not loaded: ";
pub(crate) use crate::tools::deferral::SEARCH_EMPTY_QUERY;

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

    fn to_info(&self, server_name: &str) -> McpPromptInfo {
        McpPromptInfo {
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
    tools: Vec<McpToolDef>,
    prompts: Vec<McpPromptDef>,
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
        self.tools.clear();
        self.prompts.clear();
    }

    fn populate(&mut self, result: StartResult) {
        let StartResult {
            transport,
            tool_infos,
            prompt_infos,
        } = result;
        self.tools = tool_infos
            .into_iter()
            .filter(|info| {
                if !config::is_valid_tool_name(&info.name) {
                    warn!(tool = %info.name, server = %self.name, "skipping tool with invalid name");
                    return false;
                }
                // Wire format is server__tool — check total length fits LLM API limits
                let wire_len = self.name.len() + 2 + info.name.len();
                if wire_len > 64 {
                    warn!(
                        tool = %info.name,
                        server = %self.name,
                        wire_len,
                        "skipping tool — wire name exceeds 64 char LLM API limit"
                    );
                    return false;
                }
                true
            })
            .map(|info| McpToolDef {
                qualified_name: intern(format!("{}{SEPARATOR}{}", self.name, info.name)),
                raw_name: info.name,
                description: info.description,
                input_schema: info.input_schema,
            })
            .collect();
        self.prompts = prompt_infos
            .into_iter()
            .map(|info| McpPromptDef::from_info(&self.name, info))
            .collect();
        self.transport = Some(transport);
        self.status = McpServerStatus::Running;
    }
}

struct McpManagerInner {
    entries: Vec<ServerEntry>,
    state_dir: Option<StateDir>,
    generation: u64,
}

#[derive(Default)]
struct ToolIndex {
    tools: HashMap<Arc<str>, ToolRef>,
    prompts: HashMap<String, PromptRef>,
    descriptors: Arc<[ToolDescriptor]>,
}

/// One published MCP tool. Wire name and search text are derived from
/// `definition` on demand: searches are model-paced and rare, so nothing
/// to cache.
struct ToolDescriptor {
    qualified_name: Arc<str>,
    always_load: bool,
    definition: Value,
}

impl ToolDescriptor {
    fn wire_name(&self) -> &str {
        self.definition["name"].as_str().unwrap_or_default()
    }
}

#[derive(Clone)]
struct ToolRef {
    qualified_name: Arc<str>,
    raw_name: String,
    subject: PermissionSubject,
    generation: u64,
    transport: Arc<dyn McpTransport>,
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

    pub async fn call(&self, args: &Value) -> Result<String, McpError> {
        transport::call_tool(self.tool.transport.as_ref(), &self.tool.raw_name, args).await
    }
}

struct PromptRef {
    raw_name: String,
    transport: Arc<dyn McpTransport>,
}

#[derive(Clone)]
pub struct McpPromptInfo {
    pub display_name: String,
    pub qualified_name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArg>,
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
    pub pids: Vec<u32>,
    pub generation: u64,
}

/// How one MCP tool would reach the model on the next request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolStatus {
    pub qualified_name: String,
    pub wire_name: String,
    pub server: String,
    pub disabled: bool,
    pub deferred: bool,
}

/// Read-only view of the latest published `McpSnapshot`. Handing this out instead of the
/// raw `ArcSwap` keeps outside code from publishing snapshots of its own.
#[derive(Clone)]
pub struct McpSnapshotReader(Arc<ArcSwap<McpSnapshot>>);

impl McpSnapshotReader {
    pub fn empty() -> Self {
        Self::from_snapshot(McpSnapshot::default())
    }

    pub fn from_snapshot(snapshot: McpSnapshot) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(snapshot)))
    }

    pub fn load(&self) -> Guard<Arc<McpSnapshot>> {
        self.0.load()
    }

    pub fn load_full(&self) -> Arc<McpSnapshot> {
        self.0.load_full()
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
    /// Never changes after startup, so it lives here instead of being
    /// copied into every republished `ToolIndex`.
    defer_tools: usize,
    /// Nothing is ever sent on it. `run` drops the sender once the first
    /// connect pass has published, and that disconnect is the signal.
    ready_rx: flume::Receiver<Infallible>,
}

/// One session's view of MCP: the shared handle plus the deferred tools
/// this session loaded. Loads are per session, so a subagent's searches
/// never bloat the parent's context.
///
/// `McpRequestSnapshot::extend_tools` output must never be stored: recompute it every request
/// or the `tool_search` catalog goes stale.
#[derive(Clone)]
pub struct McpSession {
    handle: McpHandle,
    loaded: Arc<Mutex<HashSet<Arc<str>>>>,
    /// The MCP-qualified entries of config's `disabled_tools`. Held here rather
    /// than in `ToolFilter` because MCP definitions are appended after the
    /// registry filter has already run.
    disabled: Arc<[String]>,
}

/// Immutable MCP state used to assemble and account for one model request.
pub struct McpRequestSnapshot {
    index: Arc<ToolIndex>,
    loaded: HashSet<Arc<str>>,
    disabled: Arc<[String]>,
    defer_tools: usize,
}

impl std::ops::Deref for McpSession {
    type Target = McpHandle;
    fn deref(&self) -> &McpHandle {
        &self.handle
    }
}

impl McpSession {
    /// `history` seeds the loaded set when resuming: tools the model was
    /// already calling stay declared across restarts. A pure string scan,
    /// so it is safe before servers connect, and unknown names are inert.
    pub fn new(handle: McpHandle, history: &[Message]) -> Self {
        let loaded = history
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|block| match block {
                ContentBlock::ToolUse { name, .. } if name.contains(WIRE_SEPARATOR) => {
                    Some(internal_tool_name(name).into())
                }
                _ => None,
            })
            .collect();
        Self {
            handle,
            loaded: Arc::new(Mutex::new(loaded)),
            disabled: Arc::from([]),
        }
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
        is_disabled(&self.disabled, qualified_name)
    }

    /// Captures the published tool generation while the session's loaded set is stable.
    pub fn request_snapshot(&self) -> McpRequestSnapshot {
        let (index, loaded) = {
            let loaded = self.lock_loaded();
            (self.handle.index.load_full(), loaded.clone())
        };
        McpRequestSnapshot {
            index,
            loaded,
            disabled: Arc::clone(&self.disabled),
            defer_tools: self.handle.defer_tools,
        }
    }

    /// A view over the same handle with no loads, for a new (sub)session.
    pub fn fresh(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            loaded: Arc::new(Mutex::new(HashSet::new())),
            disabled: Arc::clone(&self.disabled),
        }
    }

    /// Rank deferred tools against `query` keywords (exact name first,
    /// then name hits over description hits) and mark the top
    /// `MAX_SEARCH_LOADS` loaded; their definitions join the next request.
    pub fn search_tools(&self, query: &str) -> Result<SearchOutcome, String> {
        let q = query.trim().to_lowercase();
        let tokens: Vec<&str> = q
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.is_empty() {
            return Err(SEARCH_EMPTY_QUERY.into());
        }
        let idx = self.handle.index.load();
        let mut matches: Vec<(bool, usize, &ToolDescriptor)> = idx
            .descriptors
            .iter()
            .filter(|d| !d.always_load && !self.is_disabled(&d.qualified_name))
            .filter_map(|d| {
                let name = d.wire_name().to_lowercase();
                let haystack = build_haystack(&d.definition);
                // The catalog shows bare tool names, so exact match must
                // accept both `server__tool` and `tool`.
                let exact = name == q
                    || d.qualified_name
                        .split_once(SEPARATOR)
                        .is_some_and(|(_, raw)| raw.eq_ignore_ascii_case(&q));
                let score: usize = tokens
                    .iter()
                    .map(|t| {
                        if name.contains(t) {
                            NAME_HIT_SCORE
                        } else if haystack.contains(t) {
                            DESCRIPTION_HIT_SCORE
                        } else {
                            0
                        }
                    })
                    .sum();
                (exact || score > 0).then_some((exact, score, d))
            })
            .collect();
        matches.sort_by(|a, b| {
            (b.0, b.1)
                .cmp(&(a.0, a.1))
                .then_with(|| a.2.wire_name().cmp(b.2.wire_name()))
        });
        let mut guard = self.lock_loaded();
        let mut hits: Vec<&str> = Vec::new();
        let mut overflow: Vec<&str> = Vec::new();
        let mut loaded: Vec<Arc<str>> = Vec::new();
        for (_, _, d) in &matches {
            if hits.len() < MAX_SEARCH_LOADS {
                if guard.insert(Arc::clone(&d.qualified_name)) {
                    loaded.push(Arc::from(d.wire_name()));
                }
                hits.push(d.wire_name());
            } else {
                overflow.push(d.wire_name());
            }
        }
        drop(guard);
        info!(query = %q, loaded = hits.len(), overflow = overflow.len(), "MCP tool search");
        if hits.is_empty() {
            return Ok(SearchOutcome {
                loaded,
                message: format!(
                    "{SEARCH_NO_MATCH} '{query}'. Try other keywords or an exact name from the catalog."
                ),
            });
        }
        let plural = if hits.len() == 1 { "tool" } else { "tools" };
        let mut out = format!(
            "Loaded {} {plural}, callable from your next message:",
            hits.len()
        );
        for hit in &hits {
            out.push_str(&format!("\n- `{hit}`"));
        }
        if !overflow.is_empty() {
            let shown = overflow.len().min(MAX_OVERFLOW_NAMES);
            let names: Vec<String> = overflow[..shown].iter().map(|n| format!("`{n}`")).collect();
            out.push_str(&format!("\n{SEARCH_OVERFLOW_PREFIX}{}", names.join(", ")));
            if overflow.len() > shown {
                out.push_str(&format!(" and {} more", overflow.len() - shown));
            }
            out.push_str(". Search an exact tool name to load it.");
        }
        Ok(SearchOutcome {
            loaded,
            message: out,
        })
    }

    /// Invoked on every MCP dispatch: a deferred tool the model calls by
    /// catalog name gets its full definition on the next request. `true` when
    /// that call is what declared it, so the change can be reported once.
    pub fn mark_loaded(&self, qualified_name: &str) -> bool {
        self.lock_loaded().insert(Arc::from(qualified_name))
    }

    fn lock_loaded(&self) -> std::sync::MutexGuard<'_, HashSet<Arc<str>>> {
        self.loaded.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl McpRequestSnapshot {
    fn is_disabled(&self, qualified_name: &str) -> bool {
        is_disabled(&self.disabled, qualified_name)
    }

    fn deferring(&self) -> bool {
        self.index
            .descriptors
            .iter()
            .filter(|descriptor| {
                !descriptor.always_load && !self.is_disabled(&descriptor.qualified_name)
            })
            .count()
            > self.defer_tools
    }

    /// Every MCP tool in this request snapshot and how it reaches the model.
    pub fn tool_inventory(&self) -> Vec<McpToolStatus> {
        let defer = self.deferring();
        self.index
            .descriptors
            .iter()
            .map(|descriptor| {
                let disabled = self.is_disabled(&descriptor.qualified_name);
                let (server, _) = descriptor
                    .qualified_name
                    .split_once(SEPARATOR)
                    .unwrap_or((UNKNOWN_MCP, &descriptor.qualified_name));
                McpToolStatus {
                    server: server.to_owned(),
                    wire_name: descriptor.wire_name().to_owned(),
                    deferred: !disabled
                        && defer
                        && !descriptor.always_load
                        && !self.loaded.contains(&descriptor.qualified_name),
                    qualified_name: descriptor.qualified_name.to_string(),
                    disabled,
                }
            })
            .collect()
    }

    /// Append this request's MCP definitions: loaded and `always_load`
    /// tools in full, the rest as names inside one `tool_search` catalog.
    /// Names already in the array are skipped.
    ///
    /// The `defer_tools` threshold is measured against the full index, not
    /// what's left deferred, so loading tools mid-session can never flip
    /// the remainder into the context.
    pub fn extend_tools(&self, tools: &mut Value) {
        let section = self.extend_declared(tools);
        crate::tools::deferral::push_catalog(tools, section.as_slice());
    }

    /// Appends this request's declared definitions and returns MCP's slice of
    /// the shared `tool_search` entry. See [`DeferralSnapshot::extend_declared`]
    /// for why the two halves are separate.
    pub fn extend_declared(&self, tools: &mut Value) -> Option<String> {
        let Some(arr) = tools.as_array_mut() else {
            debug_assert!(false, "tools must be a JSON array");
            return None;
        };
        let existing: HashSet<String> = arr
            .iter()
            .filter_map(|t| t["name"].as_str().map(String::from))
            .collect();
        let enabled = || {
            self.index
                .descriptors
                .iter()
                .filter(|descriptor| !self.is_disabled(&descriptor.qualified_name))
        };
        let defer = self.deferring();
        let mut deferred: Vec<&ToolDescriptor> = Vec::new();
        for descriptor in enabled() {
            if existing.contains(descriptor.wire_name()) {
                continue;
            }
            if !defer || descriptor.always_load || self.loaded.contains(&descriptor.qualified_name)
            {
                arr.push(descriptor.definition.clone());
            } else {
                deferred.push(descriptor);
            }
        }
        (!deferred.is_empty()).then(|| catalog_section(&deferred))
    }
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

    pub fn reader(&self) -> McpSnapshotReader {
        McpSnapshotReader(Arc::clone(&self.snapshot))
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.index.load().tools.contains_key(name)
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

    pub async fn call_tool(&self, qualified_name: &str, args: &Value) -> Result<String, McpError> {
        self.bind_tool(qualified_name)?.call(args).await
    }

    pub async fn get_prompt(
        &self,
        qualified_name: &str,
        arguments: &HashMap<String, String>,
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
        transport::get_prompt(transport.as_ref(), &raw_name, arguments).await
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

    let defer_tools = config.defer_tools.unwrap_or(DEFAULT_DEFER_TOOLS);
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
        defer_tools,
        ready_rx,
    };

    info!(total = inner.entries.len(), "MCP servers connecting");

    smol::spawn(run(inner, index, snapshot, cmd_rx, ready_tx)).detach();
    Some(handle)
}

/// Connect results ride the same loop as commands, so a `Shutdown` arriving
/// mid-connect never waits for a slow `initialize`.
enum Step {
    Connected(usize, Result<StartResult, McpError>),
    Command(McpCommand),
    /// Every handle is gone.
    Closed,
}

async fn run(
    mut inner: McpManagerInner,
    index: Arc<ArcSwap<ToolIndex>>,
    snapshot: Arc<ArcSwap<McpSnapshot>>,
    cmd_rx: flume::Receiver<McpCommand>,
    ready_tx: flume::Sender<Infallible>,
) {
    let (connected_tx, connected_rx) = flume::unbounded();
    // Held, not detached: dropping a pending connect drops the transport it
    // owns, which kills the child process group it just spawned.
    let connects = spawn_connects(&inner, connected_tx);
    let mut ready = Some(ready_tx);

    let mut ack: Option<flume::Sender<()>> = None;
    loop {
        release_ready(&inner, &mut ready);
        let step = futures_lite::future::or(
            async {
                match connected_rx.recv_async().await {
                    Ok((i, result)) => Step::Connected(i, result),
                    // Every connect landed, so only commands wake us now.
                    Err(_) => futures_lite::future::pending().await,
                }
            },
            async {
                match cmd_rx.recv_async().await {
                    Ok(cmd) => Step::Command(cmd),
                    Err(_) => Step::Closed,
                }
            },
        )
        .await;

        match step {
            Step::Connected(i, result) => {
                // A toggle or reconnect that ran mid-connect owns the entry
                // now, so a late result must not resurrect it. Dropping it
                // kills the transport it carries.
                if inner.entries[i].status == McpServerStatus::Connecting {
                    let _ = apply_start_result(&mut inner.entries[i], result, "start");
                }
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
    if let McpError::HttpError {
        status: 401,
        reason,
        ..
    } = e
    {
        McpServerStatus::NeedsAuth {
            url: Some(reason.clone()),
        }
    } else {
        McpServerStatus::Failed(e.to_string())
    }
}

struct StartResult {
    transport: Arc<dyn McpTransport>,
    tool_infos: Vec<protocol::ToolInfo>,
    prompt_infos: Vec<protocol::PromptInfo>,
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
    let capabilities = transport::initialize(transport.as_ref()).await?;
    // Asymmetric on purpose: sloppy servers omit `capabilities` yet serve
    // tools/list fine, so always ask (fatal only when tools were declared).
    // Prompts only when declared: undeclared endpoints may answer junk,
    // and junk must not take down the server's tools.
    let tool_infos = match transport::list_tools(transport.as_ref()).await {
        Ok(tools) => tools,
        Err(e) if !capabilities.tools => {
            warn!(server = config.name, error = %e, "tools/list failed; server declared no tools");
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    let prompt_infos = if capabilities.prompts {
        transport::list_prompts(transport.as_ref()).await?
    } else {
        Vec::new()
    };
    info!(
        server = config.name,
        tool_count = tool_infos.len(),
        prompt_count = prompt_infos.len(),
        "MCP server initialized"
    );
    Ok(StartResult {
        transport,
        tool_infos,
        prompt_infos,
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
            tools: Vec::new(),
            prompts: Vec::new(),
        });
    }

    // Config maps are unordered; a stable order keeps the tool_search
    // catalog and tools array byte-identical across runs (prompt cache).
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    McpManagerInner {
        entries,
        state_dir,
        generation: 0,
    }
}

/// One task per enabled server, each reporting back as it lands, so `run`
/// publishes a fast server's tools without waiting for the slowest.
fn spawn_connects(
    inner: &McpManagerInner,
    tx: flume::Sender<(usize, Result<StartResult, McpError>)>,
) -> Vec<smol::Task<()>> {
    inner
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.status == McpServerStatus::Connecting)
        .filter(|(_, e)| entry_has_startup_trust(e, inner.state_dir.as_ref()))
        .filter_map(|(i, e)| e.config.clone().map(|c| (i, c)))
        .map(|(i, config)| {
            let tx = tx.clone();
            smol::spawn(async move {
                let _ = tx.send_async((i, start_server(&config, true).await)).await;
            })
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

        if let Some(ref transport) = entry.transport
            && entry.status != McpServerStatus::Disabled
        {
            let always_load = entry.config.as_ref().is_some_and(|c| c.always_load);
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
                    },
                );
                let sanitized_schema = sanitize_tool_input_schema(t.input_schema.clone());
                descriptors.push(ToolDescriptor {
                    qualified_name: Arc::clone(&t.qualified_name),
                    always_load,
                    definition: json!({
                        "name": wire_tool_name(&t.qualified_name),
                        "description": t.description,
                        "input_schema": sanitized_schema,
                    }),
                });
            }
            for p in &entry.prompts {
                prompts.insert(
                    p.qualified_name.clone(),
                    PromptRef {
                        raw_name: p.raw_name.clone(),
                        transport: Arc::clone(transport),
                    },
                );
                prompt_infos.push(p.to_info(&entry.name));
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
        });
    }

    index.store(Arc::new(ToolIndex {
        tools,
        prompts,
        descriptors: descriptors.into(),
    }));
    snapshot.store(Arc::new(McpSnapshot {
        infos: server_infos,
        prompts: prompt_infos,
        pids,
        generation: inner.generation,
    }));
}

/// Session for dispatch-level tests outside this module, built through the
/// real `publish` path so it can't drift from production index construction.
#[cfg(test)]
pub(crate) fn stub_session(tools: &[(&str, &str)]) -> McpSession {
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
        transport: Some(Arc::new(StubTransport(Arc::from("stub")))),
        tools: tools
            .iter()
            .map(|(qualified, description)| McpToolDef {
                qualified_name: Arc::from(*qualified),
                raw_name: qualified
                    .split_once(SEPARATOR)
                    .map_or(*qualified, |(_, r)| r)
                    .into(),
                description: (*description).into(),
                input_schema: json!({}),
            })
            .collect(),
        prompts: Vec::new(),
    };
    let inner = McpManagerInner {
        entries: vec![entry],
        state_dir: None,
        generation: 0,
    };
    let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
    let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
    publish(&inner, &index, &snapshot);
    McpSession::new(
        McpHandle {
            cmd_tx: flume::unbounded().0,
            index,
            snapshot,
            defer_tools: 0,
            ready_rx: flume::bounded(0).1,
        },
        &[],
    )
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

#[cfg(test)]
struct StubTransport(Arc<str>);

#[cfg(test)]
impl McpTransport for StubTransport {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        _params: Option<Value>,
    ) -> transport::BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(async move {
            Err(McpError::UnknownTool {
                name: method.into(),
            })
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
        &self.0
    }
    fn transport_kind(&self) -> &'static str {
        "stub"
    }
}

fn build_haystack(definition: &Value) -> String {
    let mut hay = definition["description"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    if let Some(props) = definition["input_schema"]["properties"].as_object() {
        for key in props.keys() {
            hay.push(' ');
            hay.push_str(&key.to_lowercase());
        }
    }
    hay
}

/// MCP's slice of the shared `tool_search` entry. Unlike the built-in
/// catalog this stays bare names: a server index is unbounded, so a sentence
/// each would cost more than the deferral saves.
fn catalog_section(deferred: &[&ToolDescriptor]) -> String {
    // Grouping by server drops the repeated `server__` prefix, a few
    // tokens per tool. Descriptors arrive grouped because entries are
    // sorted and published per server.
    let mut catalog = String::new();
    let mut current_server = "";
    for d in deferred {
        let (server, raw) = d
            .qualified_name
            .split_once(SEPARATOR)
            .unwrap_or((UNKNOWN_MCP, &d.qualified_name));
        if server == current_server {
            catalog.push_str(", ");
        } else {
            if !catalog.is_empty() {
                catalog.push('\n');
            }
            catalog.push_str(server);
            catalog.push_str(": ");
            current_server = server;
        }
        catalog.push_str(raw);
    }
    format!("From MCP servers:\n{catalog}")
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
    use crate::tools::deferral::TOOL_SEARCH_TOOL_NAME;
    use async_lock::Mutex as AsyncMutex;
    use caudra_providers::{Model, Role};
    use caudra_storage::sessions::SessionDatabase;
    use caudra_storage::state::project_scope;
    use config::{RawHttpFields, RawServerConfig, RawStdioFields, RawTransport};
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(unix)]
    use std::time::Instant;
    use test_case::test_case;

    use crate::context::{
        ContextCapture, ContextInventory, ContextMcpInventory, ContextReadiness, ContextSnapshot,
    };

    const DEFAULT_TIMEOUT_MS: u64 = 30_000;
    const MISSING_PROGRAM: &str = "/nonexistent/definitely-not-here";
    const ORIGINAL_DESCRIPTION: &str = "original contract";
    const REPLACEMENT_CATALOG_ENTRY: &str = "srv: replacement";
    const REPLACEMENT_DESCRIPTION: &str = "replacement contract";
    const REPLACEMENT_TOOL_NAME: &str = "srv.replacement";
    const TEST_MODEL_SPEC: &str = "anthropic/claude-sonnet-4-6";

    fn stdio_raw(cmd: &[&str]) -> RawServerConfig {
        RawServerConfig {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            always_load: false,
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
            always_load: false,
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
    const WIRE_TOOL_NAME: &str = "srv__tool";
    const BUILTIN_DEFERRED: &str = "code_map";
    const BUILTIN_DESCRIPTION: &str = "Rank every symbol in a source tree.";

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
            tools: vec![McpToolDef {
                qualified_name: qualified,
                raw_name: "tool".into(),
                description: String::new(),
                input_schema: json!({}),
            }],
            prompts: Vec::new(),
        }
    }

    fn bad_stdio_config(name: &str) -> ServerConfig {
        ServerConfig {
            local_execution: LocalExecutionPolicy::Embedded,
            name: name.into(),
            timeout: Duration::from_secs(1),
            always_load: false,
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
        setup_with_defer(entries, 0)
    }

    fn setup_with_defer(
        entries: Vec<ServerEntry>,
        defer_tools: usize,
    ) -> (McpManagerInner, McpSession) {
        let inner = McpManagerInner {
            entries,
            state_dir: None,
            generation: 0,
        };
        let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
        let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
        publish(&inner, &index, &snapshot);
        let handle = McpHandle {
            cmd_tx: flume::unbounded().0,
            index,
            snapshot,
            defer_tools,
            ready_rx: flume::bounded(0).1,
        };
        (inner, McpSession::new(handle, &[]))
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
    fn request_snapshot_survives_generation_replacement_with_attributable_definitions() {
        let original = entry_with_tools(
            "srv",
            vec![tool_def("srv", "tool", ORIGINAL_DESCRIPTION, json!({}))],
        );
        let (mut inner, session) = setup(vec![original]);
        session.mark_loaded(TOOL_NAME);
        let request = session.request_snapshot();
        assert!(session.loaded.try_lock().is_ok());

        inner.entries[0] = entry_with_tools(
            "srv",
            vec![tool_def(
                "srv",
                "replacement",
                REPLACEMENT_DESCRIPTION,
                json!({}),
            )],
        );
        inner.generation += 1;
        publish(
            &inner,
            session.handle.index.as_ref(),
            session.handle.snapshot.as_ref(),
        );

        let base_tools = json!([]);
        let mut full_tools = base_tools.clone();
        request.extend_tools(&mut full_tools);
        assert_eq!(tool_names(&full_tools), [WIRE_TOOL_NAME]);
        assert_eq!(full_tools[0]["description"], ORIGINAL_DESCRIPTION);
        assert_eq!(request.index.tools[TOOL_NAME].generation, 0);

        let model = Model::from_spec(TEST_MODEL_SPEC).unwrap();
        let captured = ContextSnapshot::capture(ContextCapture {
            readiness: ContextReadiness::CapturedCurrentRequest,
            model: &model,
            auto_compact: false,
            compaction_buffer: None,
            system: "",
            base_tools: &base_tools,
            full_tools: &full_tools,
            projected_messages: &[],
            measured: None,
            inventory: ContextInventory {
                mcp: ContextMcpInventory::from_statuses(request.tool_inventory()),
                ..ContextInventory::default()
            },
        });
        assert_eq!(captured.inventory.mcp.unattributed_tokens, 0);
        assert_eq!(captured.inventory.mcp.tools[0].wire_name, WIRE_TOOL_NAME);
        assert_eq!(
            captured.inventory.mcp.tools[0].request_tokens,
            captured.usage.mcp_tools
        );

        let current = session.request_snapshot();
        let mut current_tools = json!([]);
        current.extend_tools(&mut current_tools);
        assert_eq!(tool_names(&current_tools), [TOOL_SEARCH_TOOL_NAME]);
        assert_eq!(
            current.tool_inventory()[0].qualified_name,
            REPLACEMENT_TOOL_NAME
        );
        assert_eq!(
            current.index.tools[REPLACEMENT_TOOL_NAME].generation,
            inner.generation
        );
        assert!(
            current_tools[0]["description"]
                .as_str()
                .is_some_and(|description| description.contains(REPLACEMENT_CATALOG_ENTRY))
        );
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
        assert!(spawn_connects(&inner, tx).is_empty());
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
            assert!(spawn_connects(&inner, tx).is_empty());
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
        assert!(spawn_connects(&inner, tx).is_empty());
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

    fn always_load_entry(name: &str, transport: Arc<dyn McpTransport>) -> ServerEntry {
        let mut raw = stdio_raw(&["echo"]);
        raw.always_load = true;
        let mut entry = fake_entry(name, transport);
        entry.config = Some(parse_server(name.into(), raw).unwrap());
        entry
    }

    #[test]
    fn extend_tools_skips_deferral_at_or_below_threshold() {
        let (_inner, handle) = setup_with_defer(vec![fake_entry("srv", FakeTransport::new())], 1);
        let mut tools = json!([]);
        handle.request_snapshot().extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec![WIRE_TOOL_NAME]);
    }

    #[test_case(&["srv.tool".to_owned()] ; "qualified_name")]
    #[test_case(&["srv.*".to_owned()] ; "server_wildcard")]
    fn extend_tools_drops_disabled_tools(disabled: &[String]) {
        let (_inner, session) = setup_with_defer(vec![fake_entry("srv", FakeTransport::new())], 1);
        let mut tools = json!([]);
        session
            .with_disabled_tools(disabled)
            .request_snapshot()
            .extend_tools(&mut tools);
        assert!(tool_names(&tools).is_empty());
    }

    /// A disabled tool must not surface as a name in the catalog either: that
    /// is the whole point of turning it off.
    #[test]
    fn a_disabled_tool_stays_out_of_the_search_catalog() {
        let (_inner, session) = setup(vec![
            fake_entry("srv", FakeTransport::new()),
            fake_entry("other", FakeTransport::new()),
        ]);
        let session = session.with_disabled_tools(&["srv.*".to_owned()]);
        let mut tools = json!([]);
        session.request_snapshot().extend_tools(&mut tools);

        assert_eq!(tool_names(&tools), vec![TOOL_SEARCH_TOOL_NAME]);
        let catalog = tools[0]["description"].as_str().unwrap();
        assert!(catalog.contains("other"), "{catalog}");
        assert!(!catalog.contains("srv"), "{catalog}");
        assert!(
            session
                .search_tools("tool")
                .unwrap()
                .message
                .contains("other__tool"),
            "search must not reach a disabled tool"
        );
    }

    #[test]
    fn disabling_tools_can_bring_a_server_back_under_the_defer_threshold() {
        let (_inner, session) = setup_with_defer(
            vec![
                fake_entry("srv", FakeTransport::new()),
                fake_entry("other", FakeTransport::new()),
            ],
            1,
        );
        let mut tools = json!([]);
        session
            .with_disabled_tools(&["srv.*".to_owned()])
            .request_snapshot()
            .extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec!["other__tool"]);
    }

    #[test]
    fn defer_threshold_ignores_always_load_tools() {
        let (_inner, handle) = setup_with_defer(
            vec![
                always_load_entry("eager", FakeTransport::new()),
                fake_entry("lazy", FakeTransport::new()),
            ],
            1,
        );
        let mut tools = json!([]);
        handle.request_snapshot().extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec!["eager__tool", "lazy__tool"]);
    }

    #[test]
    fn extend_tools_defers_behind_tool_search_by_default() {
        let (_inner, handle) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let mut tools = json!([]);
        handle.request_snapshot().extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec![TOOL_SEARCH_TOOL_NAME]);
        let catalog = tools[0]["description"].as_str().unwrap();
        assert!(catalog.contains("srv: tool"), "catalog groups by server");
        assert!(handle.has_tool(TOOL_NAME), "deferred tools stay callable");
    }

    #[test]
    fn extend_tools_includes_always_load_server_upfront() {
        let (_inner, handle) = setup(vec![
            always_load_entry("eager", FakeTransport::new()),
            fake_entry("lazy", FakeTransport::new()),
        ]);
        let mut tools = json!([]);
        handle.request_snapshot().extend_tools(&mut tools);
        let names = tool_names(&tools);
        assert!(names.contains(&"eager__tool"));
        assert!(names.contains(&TOOL_SEARCH_TOOL_NAME));
        assert!(!names.contains(&"lazy__tool"));
    }

    #[test]
    fn search_loads_tools_into_next_extend() {
        let (_inner, handle) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let result = handle.search_tools("TOOL").unwrap().message;
        assert!(result.contains(WIRE_TOOL_NAME), "got: {result}");

        let mut tools = json!([]);
        handle.request_snapshot().extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec![WIRE_TOOL_NAME]);
    }

    #[test]
    fn search_reports_no_match_without_loading() {
        let (_inner, handle) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let result = handle
            .search_tools("nonexistent-capability")
            .unwrap()
            .message;
        assert!(result.contains(SEARCH_NO_MATCH), "got: {result}");
        let mut tools = json!([]);
        handle.request_snapshot().extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec![TOOL_SEARCH_TOOL_NAME]);
    }

    #[test]
    fn search_caps_loads_and_reports_overflow() {
        let transport: Arc<dyn McpTransport> = FakeTransport::new();
        let mut entry = fake_entry("srv", Arc::clone(&transport));
        entry.tools = (0..MAX_SEARCH_LOADS + 2)
            .map(|i| McpToolDef {
                qualified_name: intern(format!("srv{SEPARATOR}tool-{i}")),
                raw_name: format!("tool-{i}"),
                description: String::new(),
                input_schema: json!({}),
            })
            .collect();
        let (_inner, handle) = setup(vec![entry]);

        let result = handle.search_tools("tool").unwrap().message;
        let expected = format!(
            "{SEARCH_OVERFLOW_PREFIX}`srv__tool-{}`, `srv__tool-{}`",
            MAX_SEARCH_LOADS,
            MAX_SEARCH_LOADS + 1
        );
        assert!(
            result.contains(&expected),
            "overflow must list names: {result}"
        );
        let mut tools = json!([]);
        handle.request_snapshot().extend_tools(&mut tools);
        // Loaded cap plus the search tool for the remaining deferred ones.
        assert_eq!(tools.as_array().unwrap().len(), MAX_SEARCH_LOADS + 1);
    }

    fn entry_with_tools(name: &str, tools: Vec<McpToolDef>) -> ServerEntry {
        let mut entry = fake_entry(name, FakeTransport::new());
        entry.tools = tools;
        entry
    }

    fn tool_def(server: &str, raw: &str, description: &str, schema: Value) -> McpToolDef {
        McpToolDef {
            qualified_name: intern(format!("{server}{SEPARATOR}{raw}")),
            raw_name: raw.into(),
            description: description.into(),
            input_schema: schema,
        }
    }

    #[test_case("srv__tool-" ; "wire_name")]
    #[test_case("tool-" ; "bare_name_as_shown_in_catalog")]
    fn search_exact_name_outranks_keyword_matches(prefix: &str) {
        let tools = (0..MAX_SEARCH_LOADS + 1)
            .map(|i| tool_def("srv", &format!("tool-{i}"), "", json!({})))
            .collect();
        let (_inner, handle) = setup(vec![entry_with_tools("srv", tools)]);
        // Alphabetical tie-break alone would leave the last tool in overflow.
        let last = format!("{prefix}{MAX_SEARCH_LOADS}");
        let result = handle.search_tools(&last).unwrap().message;
        let overflow = result
            .lines()
            .find(|l| l.starts_with(SEARCH_OVERFLOW_PREFIX))
            .expect("one match past the cap must overflow");
        assert!(
            result.contains(&format!("srv__tool-{MAX_SEARCH_LOADS}"))
                && !overflow.contains(&format!("tool-{MAX_SEARCH_LOADS}")),
            "exact name must be loaded, not overflowed: {result}"
        );
    }

    #[test]
    fn search_ranks_name_hits_above_description_hits() {
        let tools = vec![
            tool_def("srv", "add_comment", "Comment on an issue", json!({})),
            tool_def("srv", "create_issue", "Open a ticket", json!({})),
        ];
        let (_inner, handle) = setup(vec![entry_with_tools("srv", tools)]);
        let result = handle.search_tools("issue").unwrap().message;
        let pos = |name: &str| {
            result
                .find(name)
                .unwrap_or_else(|| panic!("{name} must match: {result}"))
        };
        assert!(
            pos("srv__create_issue") < pos("srv__add_comment"),
            "name hit must rank above description hit: {result}"
        );
    }

    #[test]
    fn search_multi_word_query_matches_any_keyword() {
        let tools = vec![tool_def(
            "srv",
            "create_pr",
            "Open a pull request",
            json!({}),
        )];
        let (_inner, handle) = setup(vec![entry_with_tools("srv", tools)]);
        let result = handle.search_tools("pull request").unwrap().message;
        assert!(result.contains("srv__create_pr"), "got: {result}");
    }

    #[test]
    fn search_matches_schema_parameter_names() {
        let schema = json!({"type": "object", "properties": {"labels": {"type": "array"}}});
        let tools = vec![tool_def("srv", "update", "Update a thing", schema)];
        let (_inner, handle) = setup(vec![entry_with_tools("srv", tools)]);
        let result = handle.search_tools("labels").unwrap().message;
        assert!(result.contains("srv__update"), "got: {result}");
    }

    #[test]
    fn extend_tools_never_duplicates_existing_names() {
        let (_inner, handle) = setup(vec![always_load_entry("eager", FakeTransport::new())]);
        let mut tools = json!([]);
        handle.request_snapshot().extend_tools(&mut tools);
        handle.request_snapshot().extend_tools(&mut tools);
        assert_eq!(tool_names(&tools), vec!["eager__tool"]);
    }

    #[test]
    fn new_seeds_loads_only_from_wire_names_in_history() {
        let (_inner, session) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let tool_use = |name: &str| ContentBlock::tool_use("t", name, json!({}));
        let history = vec![Message {
            role: Role::Assistant,
            content: vec![
                tool_use(WIRE_TOOL_NAME),
                tool_use("read"),
                tool_use("gone__tool"),
            ],
            display_text: None,
            ..Default::default()
        }];
        let restored = McpSession::new(session.handle.clone(), &history);
        let mut tools = json!([]);
        restored.request_snapshot().extend_tools(&mut tools);
        assert_eq!(
            tool_names(&tools),
            vec![WIRE_TOOL_NAME],
            "only wire names still in the index may load"
        );
    }

    #[test]
    fn mid_session_loads_never_flip_remainder_into_context() {
        let defs = vec![
            tool_def("srv", "alpha", "", json!({})),
            tool_def("srv", "beta", "", json!({})),
            tool_def("srv", "gamma", "", json!({})),
        ];
        let (_inner, handle) = setup_with_defer(vec![entry_with_tools("srv", defs)], 2);
        handle.mark_loaded("srv.alpha");
        handle.mark_loaded("srv.beta");
        let mut tools = json!([]);
        handle.request_snapshot().extend_tools(&mut tools);
        let names = tool_names(&tools);
        assert!(names.contains(&"srv__alpha") && names.contains(&"srv__beta"));
        assert!(
            !names.contains(&"srv__gamma"),
            "threshold must compare the full index, not the remaining deferred count: {names:?}"
        );
        let catalog = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == TOOL_SEARCH_TOOL_NAME)
            .expect("tool_search must stay while any tool is deferred");
        let description = catalog["description"].as_str().unwrap();
        assert!(description.contains("srv: gamma"), "got: {description}");
    }

    /// Both sources share one `tool_search`, and the request builds it by
    /// running built-in deferral first. Whichever ran first used to claim the
    /// name and leave the other's tools uncatalogued.
    #[test_case(true; "builtin_exhausted_first")]
    #[test_case(false; "mcp_exhausted_first")]
    fn a_builtin_catalog_does_not_hide_the_deferred_mcp_tools(builtin_first: bool) {
        let defs = vec![
            tool_def("srv", "alpha", "", json!({})),
            tool_def("srv", "beta", "", json!({})),
            tool_def("srv", "gamma", "", json!({})),
        ];
        let (_inner, handle) = setup_with_defer(vec![entry_with_tools("srv", defs)], 2);
        let builtin = crate::tools::DeferralSession::new(
            vec![crate::tools::DeferredTool::new(
                BUILTIN_DEFERRED,
                None,
                json!({ "name": BUILTIN_DEFERRED, "description": BUILTIN_DESCRIPTION }),
            )],
            std::iter::empty(),
        );

        for (builtin_loaded, mcp_loaded, search_count) in [
            (false, false, 1),
            (builtin_first, !builtin_first, 1),
            (true, true, 0),
        ] {
            if builtin_loaded {
                builtin.mark_loaded(BUILTIN_DEFERRED);
            }
            if mcp_loaded {
                for name in ["srv.alpha", "srv.beta", "srv.gamma"] {
                    handle.mark_loaded(name);
                }
            }

            let mut tools = json!([]);
            let mut sections: Vec<String> = builtin
                .request_snapshot()
                .extend_declared(&mut tools)
                .into_iter()
                .collect();
            sections.extend(handle.request_snapshot().extend_declared(&mut tools));
            crate::tools::deferral::push_catalog(&mut tools, &sections);

            let catalogs: Vec<_> = tools
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool["name"] == TOOL_SEARCH_TOOL_NAME)
                .collect();
            assert_eq!(catalogs.len(), search_count);
            for catalog in catalogs {
                let description = catalog["description"].as_str().unwrap();
                assert_eq!(
                    description.contains(BUILTIN_DEFERRED),
                    !builtin_loaded,
                    "got: {description}"
                );
                assert_eq!(
                    description.contains("srv:"),
                    !mcp_loaded,
                    "got: {description}"
                );
            }
        }
    }

    /// A plugin or server owning the name is a real conflict, unlike the two
    /// internal sources that share the entry by design.
    #[test]
    fn a_tool_the_caller_declared_as_tool_search_keeps_the_name() {
        let (_inner, handle) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let mut tools = json!([{ "name": TOOL_SEARCH_TOOL_NAME, "description": "mine" }]);

        let sections: Vec<String> = handle
            .request_snapshot()
            .extend_declared(&mut tools)
            .into_iter()
            .collect();
        crate::tools::deferral::push_catalog(&mut tools, &sections);

        assert_eq!(tool_names(&tools), vec![TOOL_SEARCH_TOOL_NAME]);
        assert_eq!(tools.as_array().unwrap()[0]["description"], "mine");
    }

    #[test]
    fn mark_loaded_declares_tool_and_drops_empty_catalog() {
        let (_inner, handle) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        handle.mark_loaded(TOOL_NAME);
        let mut tools = json!([]);
        handle.request_snapshot().extend_tools(&mut tools);
        assert_eq!(
            tool_names(&tools),
            vec![WIRE_TOOL_NAME],
            "loaded tool must be declared; an empty catalog must not be advertised"
        );
    }

    #[test]
    fn existing_wire_name_stays_out_of_catalog() {
        let defs = vec![
            tool_def("srv", "alpha", "", json!({})),
            tool_def("srv", "beta", "", json!({})),
        ];
        let (_inner, handle) = setup(vec![entry_with_tools("srv", defs)]);
        let mut tools = json!([{ "name": "srv__alpha" }]);
        handle.request_snapshot().extend_tools(&mut tools);
        assert_eq!(
            tool_names(&tools),
            vec!["srv__alpha", TOOL_SEARCH_TOOL_NAME],
            "colliding name must be skipped, not deferred or re-added"
        );
        let catalog = tools[1]["description"].as_str().unwrap();
        assert!(catalog.contains("srv: beta"), "got: {catalog}");
        assert!(!catalog.contains("alpha"), "got: {catalog}");
    }

    #[test]
    fn history_seeding_handles_tool_names_with_double_underscores() {
        let defs = vec![tool_def("srv", "do__thing", "", json!({}))];
        let (_inner, session) = setup(vec![entry_with_tools("srv", defs)]);
        let history = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use("t", "srv__do__thing", json!({}))],
            display_text: None,
            ..Default::default()
        }];
        let restored = McpSession::new(session.handle.clone(), &history);
        let mut tools = json!([]);
        restored.request_snapshot().extend_tools(&mut tools);
        assert_eq!(
            tool_names(&tools),
            vec!["srv__do__thing"],
            "only the first __ is the server separator"
        );
    }

    #[test]
    fn search_ignores_always_load_tools() {
        let (_inner, handle) = setup(vec![always_load_entry("eager", FakeTransport::new())]);
        let result = handle.search_tools("tool").unwrap().message;
        assert!(
            result.contains(SEARCH_NO_MATCH),
            "always_load tools are already declared: {result}"
        );
    }

    #[test]
    fn search_loads_stay_scoped_to_their_session() {
        let (_inner, session_a) = setup(vec![fake_entry("srv", FakeTransport::new())]);
        let session_b = session_a.fresh();
        session_a.search_tools("tool").unwrap();

        let mut tools_a = json!([]);
        session_a.request_snapshot().extend_tools(&mut tools_a);
        assert_eq!(tool_names(&tools_a), vec![WIRE_TOOL_NAME]);

        let mut tools_b = json!([]);
        session_b.request_snapshot().extend_tools(&mut tools_b);
        assert_eq!(tool_names(&tools_b), vec![TOOL_SEARCH_TOOL_NAME]);
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
            let mut tools = json!([]);
            handle.request_snapshot().extend_tools(&mut tools);
            assert_eq!(tools[0]["name"], TOOL_SEARCH_TOOL_NAME);

            handle_toggle(&mut inner, "srv", false).await;
            publish(&inner, &handle.index, &handle.snapshot);

            let entry = &inner.entries[0];
            assert_eq!(t.shutdowns(), 1);
            assert!(entry.tools.is_empty());
            assert!(entry.transport.is_none());
            assert_eq!(entry.status, McpServerStatus::Disabled);
            assert!(!handle.has_tool(TOOL_NAME));
            let mut tools = json!([]);
            handle.request_snapshot().extend_tools(&mut tools);
            assert!(tools.as_array().unwrap().is_empty());
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
                smol::spawn(async move { handle.call_tool(TOOL_NAME, &json!({})).await.unwrap() })
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
}
