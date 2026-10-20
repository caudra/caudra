use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
#[cfg(test)]
use serde::Serialize;
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Value};
use url::{Host, Url};

use super::error::McpError;
use super::kind::McpServerKind;
use super::protocol::ServerDiscovery;
use crate::tools::is_builtin_tool;
use caudra_config::atomic_file::{self, AtomicFileError};
use caudra_config::interpolation::{self, Lookup};
use caudra_config::mcp::{DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS, MCP_FILE, MCP_FORMAT};
use caudra_config::migration::{self, ConfigFormat, ReadError};
use caudra_config::toml_document::set_value;
use caudra_config::{global_config_dir, is_valid_server_name};

const SERVERS_KEY: &str = "mcp";
const ENABLED_KEY: &str = "enabled";
const REMOVED_TOP_LEVEL_KEY: &str = "defer_tools";
const REMOVED_SERVER_KEY: &str = "always_load";
const REMOVED_KEY_REASON: &str = "was removed: MCP tools are reached through the `code` tool, which loads nothing up front. Delete the key";

#[derive(Debug, Clone)]
pub enum McpConfigError {
    Read { path: PathBuf, error: String },
    Parse { path: PathBuf, error: String },
}

/// Generates a compacted but still human-meaningful version of a path.
fn compact_path(path: &Path, base_path: &Path) -> String {
    let mut path_string = path.to_string_lossy().into_owned();

    if let Ok(stripped) = path.strip_prefix(base_path) {
        path_string = format!(".{}{}", std::path::MAIN_SEPARATOR, stripped.display());
    };
    if !path_string.starts_with('.')
        && let Some(home) = caudra_storage::paths::home()
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        path_string = format!("~{}{}", std::path::MAIN_SEPARATOR, stripped.display());
    };

    path_string
}

/// Wraps a `Vec` of `McpConfigError`s for compact display.
#[derive(Clone, Debug)]
pub struct McpConfigErrors {
    errors: Vec<McpConfigError>,
    initial_wd: PathBuf,
}

impl McpConfigErrors {
    pub fn new(working_directory: PathBuf) -> Self {
        McpConfigErrors {
            errors: Vec::new(),
            initial_wd: working_directory,
        }
    }

    fn add_error(&mut self, e: McpConfigError) {
        self.errors.push(e);
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }
}

impl std::fmt::Display for McpConfigErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut shown = self
            .errors
            .iter()
            .take(2)
            .map(|e: &McpConfigError| match e {
                McpConfigError::Read { path, .. } => {
                    format!("failed to read {}", compact_path(path, &self.initial_wd))
                }
                McpConfigError::Parse { path, .. } => {
                    format!("failed to parse {}", compact_path(path, &self.initial_wd))
                }
            });
        if let Some(first) = shown.next() {
            write!(f, "{}", first)?;
            for rest in shown {
                write!(f, "; {}", rest)?;
            }
        }
        let hidden = self.errors.len().saturating_sub(2);
        if hidden > 0 {
            write!(f, "; ... ({} more)", hidden)?;
        }
        Ok(())
    }
}

impl std::error::Error for McpConfigErrors {}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpServerStatus {
    Connecting,
    Running,
    AwaitingTrust,
    Disabled,
    Failed(String),
    NeedsAuth {
        url: Option<String>,
    },
    /// Signed in, and the server still answered 401 to a fresh token: a new
    /// sign-in with the same account would be refused too.
    Refused,
}

impl McpServerStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::Connecting | Self::AwaitingTrust)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum McpConfigSource {
    Global,
    #[default]
    Project,
    Runtime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpReviewSummary {
    pub command: Option<Vec<String>>,
    pub url: Option<String>,
    pub config_source: McpConfigSource,
    pub environment_names: Vec<String>,
    pub header_names: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct McpServerInfo {
    pub name: String,
    pub transport_kind: &'static str,
    pub tool_count: usize,
    pub prompt_count: usize,
    pub status: McpServerStatus,
    pub config_path: PathBuf,
    pub url: Option<String>,
    pub oauth: Option<OauthClientConfig>,
    pub resolved_addresses: Vec<IpAddr>,
    pub review: McpReviewSummary,
    pub kind: Option<McpServerKind>,
    /// The running server's `serverInfo`, negotiated protocol version,
    /// capabilities and instructions.
    pub discovery: Option<ServerDiscovery>,
}

#[derive(Deserialize, Default)]
#[cfg_attr(test, derive(Serialize))]
pub struct McpConfig {
    #[serde(skip)]
    pub local_execution: LocalExecutionPolicy,
    #[serde(default)]
    pub mcp: HashMap<String, RawServerConfig>,
    #[serde(skip)]
    pub origins: HashMap<String, PathBuf>,
    #[serde(skip)]
    pub sources: HashMap<String, McpConfigSource>,
    #[serde(skip)]
    pub project_root: Option<PathBuf>,
}

#[derive(Deserialize, Clone)]
#[cfg_attr(test, derive(Serialize))]
pub struct RawServerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(flatten)]
    pub transport: RawTransport,
}

impl RawServerConfig {
    /// Server declared at runtime instead of in `mcp.toml`.
    pub(super) fn runtime(transport: RawTransport) -> Self {
        Self {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            transport,
        }
    }
}

#[derive(Deserialize, Clone)]
#[cfg_attr(test, derive(Serialize))]
#[serde(untagged)]
pub enum RawTransport {
    Stdio(RawStdioFields),
    Http(RawHttpFields),
}

#[derive(Deserialize, Clone)]
#[cfg_attr(test, derive(Serialize))]
pub struct RawStdioFields {
    pub command: Vec<String>,
    #[serde(default)]
    pub environment: HashMap<String, String>,
}

#[derive(Deserialize, Clone)]
#[cfg_attr(test, derive(Serialize))]
pub struct RawHttpFields {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub oauth: Option<OauthClientConfig>,
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub local_execution: LocalExecutionPolicy,
    pub name: String,
    pub timeout: Duration,
    pub transport: Transport,
}

/// Static OAuth client used when the server has no registration endpoint.
#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(test, derive(Serialize))]
pub struct OauthClientConfig {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Fixed loopback port so the redirect URI can be pre-registered.
    #[serde(default)]
    pub callback_port: Option<u16>,
    /// Loopback path of the redirect URI (defaults to `/mcp/oauth/callback`).
    #[serde(default)]
    pub callback_path: Option<String>,
    /// Loopback hostname of the redirect URI (defaults to `127.0.0.1`).
    #[serde(default)]
    pub callback_hostname: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Transport {
    Stdio {
        program: String,
        args: Vec<String>,
        environment: HashMap<String, String>,
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
        oauth: Option<OauthClientConfig>,
        resolved: Vec<IpAddr>,
    },
}

pub(super) use caudra_config::is_valid_wire_name as is_valid_tool_name;

impl McpConfig {
    pub fn is_empty(&self) -> bool {
        self.mcp.is_empty()
    }

    pub fn preliminary_infos(&self, disabled: &[String]) -> Vec<McpServerInfo> {
        self.mcp
            .iter()
            .map(|(name, raw)| {
                let source = self.sources.get(name).copied().unwrap_or_default();
                let status = if !raw.enabled || disabled.contains(name) {
                    McpServerStatus::Disabled
                } else if (source == McpConfigSource::Project
                    && requires_project_trust(&raw.transport))
                    || (self.local_execution == LocalExecutionPolicy::RemoteLocal
                        && matches!(raw.transport, RawTransport::Stdio(_)))
                {
                    McpServerStatus::AwaitingTrust
                } else {
                    McpServerStatus::Connecting
                };
                McpServerInfo {
                    name: name.clone(),
                    transport_kind: transport_kind(&raw.transport),
                    tool_count: 0,
                    prompt_count: 0,
                    status,
                    config_path: self.origins.get(name).cloned().unwrap_or_default(),
                    url: match &raw.transport {
                        RawTransport::Http(h) => Some(h.url.clone()),
                        _ => None,
                    },
                    oauth: match &raw.transport {
                        RawTransport::Http(h) => h.oauth.clone(),
                        _ => None,
                    },
                    resolved_addresses: Vec::new(),
                    review: review_summary(&raw.transport, source),
                    kind: None,
                    discovery: None,
                }
            })
            .collect()
    }
}

/// Launch-context isolation only: client-local extensions remain trusted host code,
/// with unrestricted filesystem access, not sandboxed remote workspace tools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LocalExecutionPolicy {
    #[default]
    Embedded,
    RemoteLocal,
}

pub fn parse_server(name: String, server: RawServerConfig) -> Result<ServerConfig, McpError> {
    if !is_valid_server_name(&name) {
        return Err(McpError::Config(format!(
            "server name '{name}' must be ASCII alphanumeric + hyphens"
        )));
    }
    if is_builtin_tool(&name) {
        return Err(McpError::Config(format!(
            "server name '{name}' conflicts with built-in tool"
        )));
    }
    if server.timeout == 0 || server.timeout > MAX_TIMEOUT_MS {
        return Err(McpError::Config(format!(
            "server '{name}' timeout must be 1..={MAX_TIMEOUT_MS}"
        )));
    }
    let transport = match server.transport {
        RawTransport::Stdio(cfg) => {
            let mut cmd = cfg.command.into_iter();
            let program = cmd
                .next()
                .ok_or_else(|| McpError::Config(format!("server '{name}' has empty command")))?;
            Transport::Stdio {
                program,
                args: cmd.collect(),
                environment: cfg.environment,
            }
        }
        RawTransport::Http(cfg) => {
            if !cfg.url.starts_with("http://") && !cfg.url.starts_with("https://") {
                return Err(McpError::Config(format!(
                    "server '{name}' url must start with http:// or https://"
                )));
            }
            if let Some(path) = &cfg.oauth.as_ref().and_then(|o| o.callback_path.as_ref())
                && (path.is_empty() || !path.starts_with('/'))
            {
                return Err(McpError::Config(format!(
                    "server '{name}' oauth.callback_path must start with '/'"
                )));
            }
            Transport::Http {
                url: cfg.url,
                headers: cfg.headers,
                oauth: cfg.oauth,
                resolved: Vec::new(),
            }
        }
    };
    Ok(ServerConfig {
        local_execution: LocalExecutionPolicy::Embedded,
        name,
        timeout: Duration::from_millis(server.timeout),
        transport,
    })
}

pub fn transport_kind(raw: &RawTransport) -> &'static str {
    match raw {
        RawTransport::Stdio(_) => "stdio",
        RawTransport::Http(_) => "http",
    }
}

pub fn security_digest(server: &RawServerConfig) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"mcp-security-v1");
    match &server.transport {
        RawTransport::Stdio(config) => {
            hash_field(&mut hasher, b"stdio");
            hash_field(&mut hasher, b"command");
            hash_count(&mut hasher, config.command.len());
            for part in &config.command {
                hash_field(&mut hasher, part.as_bytes());
            }
            hash_map(&mut hasher, b"environment", &config.environment);
        }
        RawTransport::Http(config) => {
            hash_field(&mut hasher, b"http");
            hash_field(&mut hasher, b"url");
            hash_field(&mut hasher, config.url.as_bytes());
            hash_map(&mut hasher, b"headers", &config.headers);
            if let Some(oauth) = &config.oauth {
                hash_field(&mut hasher, b"oauth");
                hash_field(&mut hasher, oauth.client_id.as_bytes());
                hash_option(&mut hasher, oauth.client_secret.as_deref());
                hash_option(
                    &mut hasher,
                    oauth.callback_port.map(|port| port.to_string()).as_deref(),
                );
                hash_option(&mut hasher, oauth.callback_path.as_deref());
                hash_option(&mut hasher, oauth.callback_hostname.as_deref());
            } else {
                hash_field(&mut hasher, b"no-oauth");
            }
        }
    }
    let mut digest = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(&mut digest, "{byte:02x}").expect("writing to String cannot fail");
    }
    digest
}

pub fn requires_project_trust(transport: &RawTransport) -> bool {
    match transport {
        RawTransport::Stdio(_) => true,
        RawTransport::Http(config) => risky_http_url(&config.url),
    }
}

pub fn resolve_http_addresses(transport: &RawTransport) -> Result<Vec<IpAddr>, String> {
    let RawTransport::Http(config) = transport else {
        return Ok(Vec::new());
    };
    resolve_url_addresses(&config.url)
}

pub fn resolve_url_addresses(value: &str) -> Result<Vec<IpAddr>, String> {
    let url = Url::parse(value).map_err(|error| error.to_string())?;
    let host = url.host().ok_or_else(|| "URL has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL has no known port".to_string())?;
    let mut addresses = match host {
        Host::Ipv4(address) => vec![IpAddr::V4(address)],
        Host::Ipv6(address) => vec![IpAddr::V6(address)],
        Host::Domain(host) => (host, port)
            .to_socket_addrs()
            .map_err(|error| error.to_string())?
            .map(|address| address.ip())
            .collect(),
    };
    if addresses.is_empty() {
        return Err("URL host resolved to no addresses".into());
    }
    addresses.sort_unstable();
    addresses.dedup();
    Ok(addresses)
}

pub fn review_summary(
    transport: &RawTransport,
    config_source: McpConfigSource,
) -> McpReviewSummary {
    match transport {
        RawTransport::Stdio(config) => McpReviewSummary {
            command: Some(config.command.clone()),
            url: None,
            config_source,
            environment_names: sorted_names(&config.environment),
            header_names: Vec::new(),
        },
        RawTransport::Http(config) => McpReviewSummary {
            command: None,
            url: Some(safe_review_url(&config.url)),
            config_source,
            environment_names: Vec::new(),
            header_names: sorted_names(&config.headers),
        },
    }
}

fn risky_http_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return true;
    };
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return true;
    }
    match url.host() {
        Some(Host::Domain(host)) => {
            let host = host.trim_end_matches('.');
            host.eq_ignore_ascii_case("localhost")
                || host.to_ascii_lowercase().ends_with(".localhost")
        }
        Some(Host::Ipv4(address)) => risky_ip(IpAddr::V4(address)),
        Some(Host::Ipv6(address)) => risky_ip(IpAddr::V6(address)),
        None => true,
    }
}

pub fn risky_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [first, second, ..] = address.octets();
            first == 0
                || address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_multicast()
                || address.is_broadcast()
                || address.is_documentation()
                || first >= 240
                || first == 100 && (64..=127).contains(&second)
                || first == 192 && second == 0
                || first == 192 && second == 88
                || first == 198 && (18..=19).contains(&second)
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return risky_ip(IpAddr::V4(mapped));
            }
            let segments = address.segments();
            address.is_unspecified()
                || address.is_loopback()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || address.is_multicast()
                || segments[0] & 0xe000 != 0x2000
                || segments[0] == 0x2001
                    && matches!(segments[1], 0x0000 | 0x0002 | 0x0010..=0x001f | 0x0db8)
                || segments[0] == 0x2002
        }
    }
}

fn safe_review_url(value: &str) -> String {
    let Ok(mut url) = Url::parse(value) else {
        return "<invalid URL>".into();
    };
    if !url.username().is_empty() {
        let _ = url.set_username("REDACTED");
    }
    if url.password().is_some() {
        let _ = url.set_password(Some("REDACTED"));
    }
    let query_names: Vec<String> = url
        .query_pairs()
        .map(|(name, _)| name.into_owned())
        .collect();
    if !query_names.is_empty() {
        url.query_pairs_mut()
            .clear()
            .extend_pairs(query_names.iter().map(|name| (name.as_str(), "REDACTED")));
    }
    url.set_fragment(None);
    url.into()
}

fn sorted_names(values: &HashMap<String, String>) -> Vec<String> {
    let mut names: Vec<String> = values.keys().cloned().collect();
    names.sort();
    names
}

fn hash_map(hasher: &mut Sha256, label: &[u8], values: &HashMap<String, String>) {
    hash_field(hasher, label);
    hash_count(hasher, values.len());
    let mut entries: Vec<(&String, &String)> = values.iter().collect();
    entries.sort_by_key(|(name, _)| *name);
    for (name, value) in entries {
        hash_field(hasher, name.as_bytes());
        hash_field(hasher, value.as_bytes());
    }
}

fn hash_count(hasher: &mut Sha256, count: usize) {
    hasher.update((count as u64).to_be_bytes());
}

fn hash_option(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hash_field(hasher, b"some");
            hash_field(hasher, value.as_bytes());
        }
        None => hash_field(hasher, b"none"),
    }
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

/// Call order is precedence: the caller merges global first, project last,
/// so a project's servers beat the global ones.
fn merge_config(
    merged: &mut McpConfig,
    errors: &mut McpConfigErrors,
    path: &Path,
    source: McpConfigSource,
) {
    match read_config(path, source) {
        Ok(None) => {}
        Ok(Some(cfg)) => {
            tracing::info!(
                path = %path.display(),
                servers = cfg.mcp.len(),
                "loaded mcp config"
            );
            for name in cfg.mcp.keys() {
                merged.origins.insert(name.clone(), path.to_path_buf());
                merged.sources.insert(name.clone(), source);
            }
            merged.mcp.extend(cfg.mcp);
        }
        Err(e) => errors.add_error(e),
    }
}

pub fn load_config(cwd: &Path) -> (McpConfig, McpConfigErrors) {
    load_config_from(cwd, global_config_dir().as_deref())
}

pub(super) fn load_config_from(
    cwd: &Path,
    global_dir: Option<&Path>,
) -> (McpConfig, McpConfigErrors) {
    let mut merged = McpConfig {
        project_root: cwd.canonicalize().ok(),
        ..Default::default()
    };
    let mut errors = McpConfigErrors::new(cwd.to_path_buf());

    if let Some(global_dir) = global_dir {
        let global_path = global_dir.join(MCP_FILE);
        merge_config(
            &mut merged,
            &mut errors,
            &global_path,
            McpConfigSource::Global,
        );
    }
    let project_path = cwd.join(".caudra").join(MCP_FILE);
    merge_config(
        &mut merged,
        &mut errors,
        &project_path,
        McpConfigSource::Project,
    );
    (merged, errors)
}

/// `load_config` for a reload. One unreadable or invalid file fails the whole
/// load, so the caller keeps the servers it runs instead of dropping that file's.
pub fn try_load_config(cwd: &Path) -> Result<McpConfig, McpConfigErrors> {
    complete(load_config(cwd))
}

/// `try_load_config` for a manager started from `load_global_config`.
pub fn try_load_global_config(cwd: &Path) -> Result<McpConfig, McpConfigErrors> {
    complete(load_global_config(cwd))
}

pub(super) fn complete(
    (config, errors): (McpConfig, McpConfigErrors),
) -> Result<McpConfig, McpConfigErrors> {
    if errors.is_empty() {
        Ok(config)
    } else {
        Err(errors)
    }
}

pub fn load_global_config(cwd: &Path) -> (McpConfig, McpConfigErrors) {
    load_global_config_from(cwd, global_config_dir().as_deref())
}

pub(super) fn load_global_config_from(
    cwd: &Path,
    global_dir: Option<&Path>,
) -> (McpConfig, McpConfigErrors) {
    let mut merged = McpConfig {
        local_execution: LocalExecutionPolicy::RemoteLocal,
        ..Default::default()
    };
    let mut errors = McpConfigErrors::new(cwd.to_path_buf());
    if let Some(global_dir) = global_dir {
        merge_config(
            &mut merged,
            &mut errors,
            &global_dir.join(MCP_FILE),
            McpConfigSource::Global,
        );
    }
    (merged, errors)
}

pub fn http_endpoints(config: &McpConfig) -> impl Iterator<Item = &str> {
    config
        .mcp
        .values()
        .filter_map(|server| match &server.transport {
            RawTransport::Http(http) => Some(http.url.as_str()),
            RawTransport::Stdio(_) => None,
        })
}

/// Turns `server_name` on or off in the file it came from. A global file waiting for its format
/// upgrade is refused; a project file is edited at whatever version it declares.
pub fn persist_enabled(
    config_path: &Path,
    source: McpConfigSource,
    server_name: &str,
    enabled: bool,
) -> Result<(), McpError> {
    let guard = (source == McpConfigSource::Global).then_some(&MCP_FORMAT);
    write_enabled(config_path, guard, server_name, enabled)
}

fn write_enabled(
    config_path: &Path,
    guard: Option<&ConfigFormat>,
    server_name: &str,
    enabled: bool,
) -> Result<(), McpError> {
    let file_error = |error: AtomicFileError| McpError::Config(error.to_string());
    let bytes = atomic_file::read(config_path).map_err(file_error)?;
    let revision = atomic_file::revision_of(bytes.as_deref());
    let content = bytes
        .map(String::from_utf8)
        .transpose()
        .map_err(|_| McpError::Config(format!("{} is not valid UTF-8", config_path.display())))?
        .unwrap_or_default();
    let mut doc: DocumentMut = content
        .parse()
        .map_err(|e| McpError::Config(format!("failed to parse {}: {e}", config_path.display())))?;
    if let Some(format) = guard {
        migration::ensure_current(&doc, format).map_err(|e| McpError::Config(e.to_string()))?;
    }

    let mcp = doc
        .entry("mcp")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let server = mcp
        .as_table_like_mut()
        .ok_or_else(|| McpError::Config("[mcp] is not a table".into()))?
        .entry(server_name)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| McpError::Config(format!("[mcp.{server_name}] is not a table")))?;
    set_value(server, ENABLED_KEY, Value::from(enabled));
    atomic_file::commit(config_path, &revision, doc.to_string().as_bytes())
        .map(drop)
        .map_err(file_error)
}

/// Whether `text`, the global MCP file at `path` in the latest format, loads.
pub fn validate_global(path: &Path, text: &str) -> Result<(), String> {
    parse(text, Lookup::Global(path)).map(drop)
}

/// Whether `text`, a project MCP file in the latest format, loads.
pub fn validate_project(_path: &Path, text: &str) -> Result<(), String> {
    parse(text, Lookup::Refused).map(drop)
}

/// Only the global file may read the environment: a project file runs no
/// review before an HTTPS server connects, so it could send a secret anywhere.
/// The first key `content` sets that code mode removed, spelled as the file spells it. A file
/// that still tunes deferral would otherwise load as though it did something.
fn removed_key(content: &str) -> Option<String> {
    let table: toml::Table = toml::from_str(content).ok()?;
    if table.contains_key(REMOVED_TOP_LEVEL_KEY) {
        return Some(REMOVED_TOP_LEVEL_KEY.to_owned());
    }
    table
        .get(SERVERS_KEY)?
        .as_table()?
        .iter()
        .find(|(_, server)| {
            server
                .as_table()
                .is_some_and(|server| server.contains_key(REMOVED_SERVER_KEY))
        })
        .map(|(name, _)| format!("{SERVERS_KEY}.{name}.{REMOVED_SERVER_KEY}"))
}

fn read_config(path: &Path, source: McpConfigSource) -> Result<Option<McpConfig>, McpConfigError> {
    let lookup = match source {
        McpConfigSource::Global => Lookup::Global(path),
        McpConfigSource::Project | McpConfigSource::Runtime => Lookup::Refused,
    };
    let content = match migration::read(path, &MCP_FORMAT) {
        Ok(Some(loaded)) => loaded.text,
        Ok(None) => {
            tracing::info!(
                path = %path.display(),
                "no mcp config to read"
            );
            return Ok(None);
        }
        Err(ReadError::Io { source, .. }) => {
            tracing::error!(
                path = %path.display(),
                error = %source,
                "failed to read mcp config"
            );
            return Err(McpConfigError::Read {
                path: path.into(),
                error: source.to_string(),
            });
        }
        Err(ReadError::Invalid { source, .. }) => {
            tracing::warn!(
                path = %path.display(),
                error = %source,
                "failed to upgrade mcp config"
            );
            return Err(McpConfigError::Parse {
                path: path.into(),
                error: source.to_string(),
            });
        }
    };
    parse(&content, lookup)
        .inspect_err(|e| {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to parse mcp config")
        })
        .map_err(|error| McpConfigError::Parse {
            path: path.into(),
            error,
        })
        .map(Some)
}

fn parse(content: &str, lookup: Lookup<'_>) -> Result<McpConfig, String> {
    if let Some(key) = removed_key(content) {
        return Err(format!("`{key}` {REMOVED_KEY_REASON}"));
    }
    interpolation::from_str(content, lookup).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use super::*;
    use caudra_config::ConfigField;
    use caudra_config::config_version::{CONFIG_VERSION_KEY, ConfigVersionError};
    use caudra_config::example::mcp::{HTTP_SERVER, OAUTH_CLIENT, STDIO_SERVER};
    use caudra_config::example::{self, Render};
    use caudra_config::mcp::{
        DEFAULT_CALLBACK_HOSTNAME, DEFAULT_CALLBACK_PATH, HTTP_FIELDS, MCP_VERSION, OAUTH_FIELDS,
        SERVER_FIELDS, STDIO_FIELDS,
    };
    use caudra_config::migration::MigrationError;
    use serde_json::Value;
    use test_case::test_case;

    const GLOBAL_SERVER: &str = "global-server";
    const PROJECT_CANARY: &str = "project-canary";
    const SERVERS_TABLE: &str = "mcp";
    const TOKEN_VARIABLE: &str = "CAUDRA_TEST_MCP_TOKEN";
    const TOKEN_VALUE: &str = "test-token";

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
                url: url.to_string(),
                headers: HashMap::new(),
                oauth: None,
            }),
        }
    }

    #[test_case("srv",       stdio_raw(&[]),            "empty command"        ; "empty_command")]
    #[test_case("shell",     stdio_raw(&["echo"]),      "conflicts with built-in" ; "builtin_name_collision")]
    #[test_case("bad name!", stdio_raw(&["echo"]),      "ASCII alphanumeric"   ; "invalid_server_name")]
    #[test_case("srv",       http_raw("ftp://bad.com"), "http://"              ; "invalid_http_url")]
    fn parse_server_rejects(name: &str, cfg: RawServerConfig, expected_msg: &str) {
        let err = parse_server(name.into(), cfg).unwrap_err();
        assert!(err.to_string().contains(expected_msg), "got: {err}");
    }

    #[test_case(0               ; "zero")]
    #[test_case(MAX_TIMEOUT_MS + 1 ; "over_max")]
    fn invalid_timeout_rejected(timeout: u64) {
        let mut cfg = stdio_raw(&["echo"]);
        cfg.timeout = timeout;
        let err = parse_server("srv".into(), cfg).unwrap_err();
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn global_only_loader_does_not_read_project_mcp_config() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let project = root.path().join("project");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(project.join(".caudra")).unwrap();
        fs::write(
            global.join(MCP_FILE),
            format!("[mcp.{GLOBAL_SERVER}]\ncommand = ['true']\n"),
        )
        .unwrap();
        fs::write(
            project.join(".caudra").join(MCP_FILE),
            format!("[mcp.{PROJECT_CANARY}]\ncommand = ['false']\n"),
        )
        .unwrap();

        let (config, errors) = load_global_config_from(&project, Some(&global));

        assert!(errors.is_empty());
        assert!(config.mcp.contains_key(GLOBAL_SERVER));
        assert!(!config.mcp.contains_key(PROJECT_CANARY));
        assert_eq!(
            config.sources.get(GLOBAL_SERVER),
            Some(&McpConfigSource::Global)
        );
    }

    #[test_case("this is not valid toml {{" ; "invalid_toml")]
    #[test_case("[mcp.broken]\ncommand = ['{env:CAUDRA_TEST_MCP_RELOAD_UNSET}']\n" ; "unset_reference")]
    fn a_reload_load_fails_whole_when_one_file_fails(global_toml: &str) {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let project = root.path().join("project");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(project.join(".caudra")).unwrap();
        fs::write(global.join(MCP_FILE), global_toml).unwrap();
        fs::write(
            project.join(".caudra").join(MCP_FILE),
            format!("[mcp.{PROJECT_CANARY}]\ncommand = ['true']\n"),
        )
        .unwrap();

        let (partial, _) = load_config_from(&project, Some(&global));
        let Err(errors) = complete(load_config_from(&project, Some(&global))) else {
            panic!("a broken global file must fail the reload load");
        };

        assert!(partial.mcp.contains_key(PROJECT_CANARY));
        assert!(errors.to_string().contains(MCP_FILE));
    }

    #[test_case("defer_tools = 30\n[mcp.srv]\ncommand = [\"x\"]\n", Some(REMOVED_TOP_LEVEL_KEY) ; "defer_tools")]
    #[test_case("[mcp.srv]\ncommand = [\"x\"]\nalways_load = true\n", Some(REMOVED_SERVER_KEY) ; "always_load")]
    #[test_case("[mcp.srv]\ncommand = [\"x\"]\n", None ; "current_file")]
    fn removed_keys_fail_the_file(document: &str, removed: Option<&str>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MCP_FILE);
        fs::write(&path, document).unwrap();
        let read = read_config(&path, McpConfigSource::Global);
        match removed {
            Some(key) => assert!(matches!(
                read,
                Err(McpConfigError::Parse { error, .. }) if error.contains(key) && error.contains(REMOVED_KEY_REASON)
            )),
            None => assert!(read.is_ok_and(|config| config.is_some())),
        }
    }

    #[test]
    fn parse_splits_command_into_program_and_args() {
        let result = parse_server("srv".into(), stdio_raw(&["npx", "-y", "server"])).unwrap();
        match &result.transport {
            Transport::Stdio { program, args, .. } => {
                assert_eq!(program, "npx");
                assert_eq!(args, &["-y", "server"]);
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn toml_deserialization() {
        let toml_str = r#"
[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.github]
command = ["gh", "mcp-server"]
environment = { GITHUB_TOKEN = "tok" }
timeout = 10000
enabled = false

[mcp.remote]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer tok123" }
"#;
        let config: McpConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mcp.len(), 3);

        assert!(matches!(
            config.mcp["filesystem"].transport,
            RawTransport::Stdio(_)
        ));

        let gh_cfg = &config.mcp["github"];
        assert!(!gh_cfg.enabled);
        assert_eq!(gh_cfg.timeout, 10000);
        match &gh_cfg.transport {
            RawTransport::Stdio(s) => assert_eq!(s.environment["GITHUB_TOKEN"], "tok"),
            _ => panic!("expected Stdio"),
        }

        match &config.mcp["remote"].transport {
            RawTransport::Http(h) => {
                assert_eq!(h.url, "https://mcp.example.com/mcp");
                assert_eq!(h.headers["Authorization"], "Bearer tok123");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn oauth_client_config_deserializes() {
        let toml_str = r#"
[mcp.acme]
url = "https://mcp.acme.example.com/mcp"
oauth = { client_id = "acme-client", client_secret = "s3cret", callback_port = 3118, callback_path = "/callback" }
"#;
        let config: McpConfig = toml::from_str(toml_str).unwrap();
        let parsed = parse_server("acme".into(), config.mcp["acme"].clone()).unwrap();
        match parsed.transport {
            Transport::Http { url, oauth, .. } => {
                assert_eq!(url, "https://mcp.acme.example.com/mcp");
                let oauth = oauth.unwrap();
                assert_eq!(oauth.client_id, "acme-client");
                assert_eq!(oauth.client_secret.as_deref(), Some("s3cret"));
                assert_eq!(oauth.callback_port, Some(3118));
                assert_eq!(oauth.callback_path.as_deref(), Some("/callback"));
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn oauth_client_secret_and_port_optional() {
        let config: McpConfig = toml::from_str(
            "[mcp.acme]\nurl = \"https://mcp.acme.example.com/mcp\"\noauth = { client_id = \"acme-client\" }\n",
        )
        .unwrap();
        let parsed = parse_server("acme".into(), config.mcp["acme"].clone()).unwrap();
        match parsed.transport {
            Transport::Http { oauth, .. } => {
                let oauth = oauth.unwrap();
                assert_eq!(oauth.client_secret, None);
                assert_eq!(oauth.callback_port, None);
                assert_eq!(oauth.callback_path, None);
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn oauth_callback_path_must_start_with_slash() {
        let config: McpConfig = toml::from_str(
            "[mcp.acme]\nurl = \"https://mcp.acme.example.com/mcp\"\noauth = { client_id = \"acme-client\", callback_path = \"callback\" }\n",
        )
        .unwrap();
        let err = parse_server("acme".into(), config.mcp["acme"].clone()).unwrap_err();
        assert!(err.to_string().contains("callback_path"));
    }

    #[test]
    fn project_config_overrides_global() {
        let dir = tempfile::tempdir().unwrap();
        let global_dir = dir.path().join("global");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(
            global_dir.join("mcp.toml"),
            r#"[mcp.srv]
command = ["global"]
timeout = 5000
"#,
        )
        .unwrap();

        let project_dir = dir.path().join("project");
        fs::create_dir_all(&project_dir).unwrap();
        let project_caudra_dir = project_dir.join(".caudra");
        fs::create_dir_all(&project_caudra_dir).unwrap();
        fs::write(
            project_caudra_dir.join("mcp.toml"),
            r#"[mcp.srv]
command = ["project"]
"#,
        )
        .unwrap();

        let project_cfg = read_config(
            &project_caudra_dir.join("mcp.toml"),
            McpConfigSource::Project,
        )
        .unwrap()
        .unwrap();
        let global_cfg = read_config(&global_dir.join("mcp.toml"), McpConfigSource::Global)
            .unwrap()
            .unwrap();

        let mut merged = McpConfig::default();
        merged.mcp.extend(global_cfg.mcp);
        merged.mcp.extend(project_cfg.mcp);

        let all: Vec<_> = merged
            .mcp
            .into_iter()
            .filter(|(_, v)| v.enabled)
            .map(|(name, cfg)| parse_server(name, cfg))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(all.len(), 1);
        match &all[0].transport {
            Transport::Stdio { program, .. } => assert_eq!(program, "project"),
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn persist_enabled_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.toml");

        persist_enabled(&path, McpConfigSource::Global, "srv", false).unwrap();
        let doc: toml_edit::DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["mcp"]["srv"]["enabled"].as_bool(), Some(false));

        fs::write(
            &path,
            r#"[mcp.srv]
command = ["echo"]
timeout = 5000
enabled = true
"#,
        )
        .unwrap();
        persist_enabled(&path, McpConfigSource::Global, "srv", false).unwrap();
        let doc: toml_edit::DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["mcp"]["srv"]["enabled"].as_bool(), Some(false));
        assert!(doc["mcp"]["srv"]["command"].is_array());
        assert_eq!(doc["mcp"]["srv"]["timeout"].as_integer(), Some(5000));
    }

    #[test_case("[mcp.srv] # server note\ncommand = [\"echo\"] # command note\nenabled = true # toggle note\n", "[mcp.srv] # server note\ncommand = [\"echo\"] # command note\nenabled = false # toggle note\n"; "table")]
    #[test_case("[mcp]\nsrv = { command = [\"echo\"], enabled = true } # server note\n", "[mcp]\nsrv = { command = [\"echo\"], enabled = false } # server note\n"; "inline_table")]
    fn persist_enabled_keeps_comments(source: &str, expected: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MCP_FILE);
        fs::write(&path, source).unwrap();

        persist_enabled(&path, McpConfigSource::Global, "srv", false).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
    }

    #[test]
    fn preliminary_infos_statuses() {
        let mut off = stdio_raw(&["echo"]);
        off.enabled = false;
        let config = McpConfig {
            mcp: [
                ("enabled".into(), stdio_raw(&["echo"])),
                ("disabled-config".into(), off),
                ("disabled-runtime".into(), stdio_raw(&["echo"])),
            ]
            .into(),
            origins: [("enabled".into(), PathBuf::from("/test.toml"))].into(),
            sources: [("enabled".into(), McpConfigSource::Global)].into(),
            ..Default::default()
        };
        let mut infos = config.preliminary_infos(&["disabled-runtime".into()]);
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(infos.len(), 3);
        assert_eq!(infos[0].status, McpServerStatus::Disabled);
        assert_eq!(infos[1].status, McpServerStatus::Disabled);
        assert_eq!(infos[2].status, McpServerStatus::Connecting);
        assert_eq!(infos[2].config_path, PathBuf::from("/test.toml"));
    }

    #[test]
    fn merge_config_project_wins() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let project = dir.path().join("project.toml");
        fs::write(&global, "[mcp.srv]\ncommand = [\"a\"]").unwrap();
        fs::write(&project, "[mcp.srv]\ncommand = [\"b\"]").unwrap();

        let mut merged = McpConfig::default();
        let mut errors = McpConfigErrors::new(dir.path().to_path_buf());
        merge_config(&mut merged, &mut errors, &global, McpConfigSource::Global);
        merge_config(&mut merged, &mut errors, &project, McpConfigSource::Project);

        assert!(errors.is_empty());
        assert_eq!(merged.origins["srv"], project, "later config must win");
        assert_eq!(merged.sources["srv"], McpConfigSource::Project);
    }

    #[test_case("http://example.com/mcp", true ; "plain_http")]
    #[test_case("https://user:pass@example.com/mcp", true ; "url_credentials")]
    #[test_case("https://localhost/mcp", true ; "localhost")]
    #[test_case("https://localhost./mcp", true ; "localhost_trailing_dot")]
    #[test_case("https://127.0.0.1/mcp", true ; "ipv4_loopback")]
    #[test_case("https://10.0.0.1/mcp", true ; "ipv4_private")]
    #[test_case("https://169.254.1.1/mcp", true ; "ipv4_link_local")]
    #[test_case("https://0.0.0.0/mcp", true ; "ipv4_unspecified")]
    #[test_case("https://[::1]/mcp", true ; "ipv6_loopback")]
    #[test_case("https://[fd00::1]/mcp", true ; "ipv6_private")]
    #[test_case("https://[fe80::1]/mcp", true ; "ipv6_link_local")]
    #[test_case("https://[::]/mcp", true ; "ipv6_unspecified")]
    #[test_case("https://example.com/mcp", false ; "public_https")]
    #[test_case("https://internal.example/mcp", false ; "domain_is_classified_after_resolution")]
    fn risky_http_classification(url: &str, expected: bool) {
        assert_eq!(requires_project_trust(&http_raw(url).transport), expected);
    }

    #[test_case("https://8.8.8.8/mcp", false ; "public_ipv4")]
    #[test_case("https://10.0.0.1/mcp", true ; "private_ipv4")]
    #[test_case("https://192.0.2.1/mcp", true ; "documentation_ipv4")]
    #[test_case("https://[2606:4700:4700::1111]/mcp", false ; "public_ipv6")]
    #[test_case("https://[2001:db8::1]/mcp", true ; "documentation_ipv6")]
    fn resolved_http_address_classification(url: &str, expected_risky: bool) {
        let addresses = resolve_http_addresses(&http_raw(url).transport).unwrap();
        assert_eq!(addresses.iter().copied().any(risky_ip), expected_risky);
    }

    #[test]
    fn security_digest_is_stable_and_tracks_security_config_only() {
        let mut config = stdio_raw(&["runner", "--safe"]);
        let RawTransport::Stdio(stdio) = &mut config.transport else {
            unreachable!();
        };
        stdio.environment.insert("TOKEN".into(), "secret".into());
        stdio.environment.insert("MODE".into(), "readonly".into());
        let digest = security_digest(&config);

        config.enabled = false;
        config.timeout = 1;
        assert_eq!(security_digest(&config), digest);

        let RawTransport::Stdio(stdio) = &mut config.transport else {
            unreachable!();
        };
        stdio.environment.insert("TOKEN".into(), "changed".into());
        assert_ne!(security_digest(&config), digest);

        let command_only = stdio_raw(&["runner", "--safe", "MODE", "readonly", "TOKEN", "secret"]);
        assert_ne!(security_digest(&command_only), digest);
    }

    #[test]
    fn review_summaries_expose_names_without_secret_values() {
        let stdio = RawTransport::Stdio(RawStdioFields {
            command: vec!["runner".into(), "serve".into()],
            environment: HashMap::from([
                ("TOKEN".into(), "environment-secret".into()),
                ("MODE".into(), "readonly".into()),
            ]),
        });
        let summary = review_summary(&stdio, McpConfigSource::Project);
        assert_eq!(summary.command, Some(vec!["runner".into(), "serve".into()]));
        assert_eq!(summary.environment_names, vec!["MODE", "TOKEN"]);
        assert_eq!(summary.config_source, McpConfigSource::Project);
        assert!(!format!("{summary:?}").contains("environment-secret"));

        let http = RawTransport::Http(RawHttpFields {
            url: "https://user:url-secret@example.com/mcp?token=query-secret#fragment-secret"
                .into(),
            headers: HashMap::from([("Authorization".into(), "header-secret".into())]),
            oauth: None,
        });
        let summary = review_summary(&http, McpConfigSource::Runtime);
        let rendered = format!("{summary:?}");
        assert_eq!(summary.header_names, vec!["Authorization"]);
        assert_eq!(summary.config_source, McpConfigSource::Runtime);
        assert!(summary.url.as_deref().unwrap().contains("REDACTED"));
        assert!(!rendered.contains("url-secret"));
        assert!(!rendered.contains("query-secret"));
        assert!(!rendered.contains("fragment-secret"));
        assert!(!rendered.contains("header-secret"));
    }

    #[test]
    fn merge_records_global_and_project_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let project = dir.path().join("project.toml");
        fs::write(&global, "[mcp.global]\ncommand = [\"global\"]").unwrap();
        fs::write(&project, "[mcp.project]\ncommand = [\"project\"]").unwrap();
        let mut config = McpConfig::default();
        let mut errors = McpConfigErrors::new(dir.path().to_path_buf());

        merge_config(&mut config, &mut errors, &global, McpConfigSource::Global);
        merge_config(&mut config, &mut errors, &project, McpConfigSource::Project);

        assert!(errors.is_empty());
        assert_eq!(config.sources["global"], McpConfigSource::Global);
        assert_eq!(config.sources["project"], McpConfigSource::Project);
    }

    #[test]
    fn read_config_directory_path_returns_read_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.toml");
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            read_config(&path, McpConfigSource::Global),
            Err(McpConfigError::Read { .. })
        ));
    }

    #[test]
    fn read_config_invalid_toml_returns_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.toml");
        fs::write(&path, "this is not valid toml {{").unwrap();
        assert!(matches!(
            read_config(&path, McpConfigSource::Global),
            Err(McpConfigError::Parse { .. })
        ));
    }

    #[test]
    fn read_config_valid_toml_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.toml");
        fs::write(
            &path,
            r#"[mcp.valid]
command = ["echo", "hello"]
"#,
        )
        .unwrap();
        let cfg = read_config(&path, McpConfigSource::Global)
            .unwrap()
            .unwrap();
        assert!(cfg.mcp.contains_key("valid"));
    }

    fn write_header_reference(dir: &Path) -> PathBuf {
        let path = dir.join(MCP_FILE);
        fs::write(
            &path,
            format!(
                "[mcp.{GLOBAL_SERVER}]\nurl = 'https://mcp.example.test'\nheaders = {{ Authorization = 'Bearer {{env:{TOKEN_VARIABLE}}}' }}\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn global_config_reads_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_header_reference(dir.path());
        fs::write(
            dir.path().join(".env"),
            format!("{TOKEN_VARIABLE}={TOKEN_VALUE}\n"),
        )
        .unwrap();

        let config = read_config(&path, McpConfigSource::Global)
            .unwrap()
            .unwrap();

        let RawTransport::Http(http) = &config.mcp[GLOBAL_SERVER].transport else {
            panic!("expected an HTTP server");
        };
        assert_eq!(
            http.headers["Authorization"],
            format!("Bearer {TOKEN_VALUE}")
        );
    }

    #[test]
    fn project_config_cannot_read_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_header_reference(dir.path());
        fs::write(
            dir.path().join(".env"),
            format!("{TOKEN_VARIABLE}={TOKEN_VALUE}\n"),
        )
        .unwrap();

        let error = read_config(&path, McpConfigSource::Project)
            .err()
            .expect("a project file is refused");

        assert!(matches!(
            &error,
            McpConfigError::Parse { error, .. }
                if error.contains(TOKEN_VARIABLE) && !error.contains(TOKEN_VALUE)
        ));
    }

    #[test]
    fn persist_enabled_refuses_a_global_file_behind_its_format() {
        const BEHIND: ConfigFormat = ConfigFormat {
            file: MCP_FILE,
            latest: 2,
            steps: &[],
            requires_version: false,
        };
        const ON_DISK: &str = "[mcp.srv]\ncommand = [\"echo\"]\nenabled = true\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MCP_FILE);
        fs::write(&path, ON_DISK).unwrap();

        let error = write_enabled(&path, Some(&BEHIND), "srv", false).unwrap_err();

        assert!(
            error
                .to_string()
                .contains(&MigrationError::Pending { file: MCP_FILE }.to_string()),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), ON_DISK);
    }

    #[test]
    fn persist_enabled_leaves_a_project_file_at_its_own_version() {
        const ON_DISK: &str = "[mcp.srv]\ncommand = [\"echo\"]\nenabled = true\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MCP_FILE);
        fs::write(&path, ON_DISK).unwrap();

        persist_enabled(&path, McpConfigSource::Project, "srv", false).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            ON_DISK.replace("enabled = true", "enabled = false")
        );
    }

    #[test]
    fn persist_enabled_keeps_references() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_header_reference(dir.path());

        persist_enabled(&path, McpConfigSource::Global, GLOBAL_SERVER, false).unwrap();

        let reference = format!("{{env:{TOKEN_VARIABLE}}}");
        assert!(fs::read_to_string(&path).unwrap().contains(&reference));
    }

    fn write_versioned_config(version: u32) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MCP_FILE);
        fs::write(
            &path,
            format!(
                "{CONFIG_VERSION_KEY} = {version}\n[mcp.{GLOBAL_SERVER}]\ncommand = ['true']\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn read_config_loads_servers_from_the_current_version() {
        let (_dir, path) = write_versioned_config(MCP_VERSION);

        let config = read_config(&path, McpConfigSource::Global)
            .unwrap()
            .unwrap();

        assert!(config.mcp.contains_key(GLOBAL_SERVER));
    }

    #[test]
    fn read_config_refuses_a_newer_version() {
        let found = MCP_VERSION + 1;
        let (_dir, path) = write_versioned_config(found);
        let expected = ConfigVersionError::Newer {
            found: i64::from(found),
            latest: MCP_VERSION,
        }
        .to_string();

        assert!(matches!(
            read_config(&path, McpConfigSource::Global),
            Err(McpConfigError::Parse { error, .. }) if error.contains(&expected)
        ));
    }

    fn load_reference(render: Render) -> McpConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MCP_FILE);
        fs::write(&path, example::mcp::document().render(render)).unwrap();
        read_config(&path, McpConfigSource::Global)
            .unwrap()
            .unwrap()
    }

    /// What Caudra acts on, with each unset option replaced by the default
    /// Caudra uses in its place.
    fn effective(mut config: McpConfig) -> Value {
        for server in config.mcp.values_mut() {
            if let RawTransport::Http(RawHttpFields {
                oauth: Some(oauth), ..
            }) = &mut server.transport
            {
                oauth
                    .callback_path
                    .get_or_insert_with(|| DEFAULT_CALLBACK_PATH.to_owned());
                oauth
                    .callback_hostname
                    .get_or_insert_with(|| DEFAULT_CALLBACK_HOSTNAME.to_owned());
            }
        }
        serde_json::to_value(&config).unwrap()
    }

    fn json_pointer(header: &str) -> String {
        header
            .split('.')
            .filter(|segment| !segment.is_empty())
            .map(|segment| format!("/{segment}"))
            .collect()
    }

    #[test]
    fn the_reference_adds_no_server() {
        let config = load_reference(Render::Reference);
        assert!(config.mcp.is_empty());
    }

    #[test]
    fn the_reference_states_the_real_defaults() {
        assert_eq!(
            effective(load_reference(Render::Live { defaults: true })),
            effective(load_reference(Render::Live { defaults: false }))
        );
    }

    #[test]
    fn the_reference_samples_are_valid_servers() {
        let config = load_reference(Render::Live { defaults: false });
        assert!(!config.mcp.is_empty());
        for (name, server) in config.mcp {
            if let Err(error) = parse_server(name.clone(), server) {
                panic!("{name}: {error}");
            }
        }
    }

    #[test_case("", &[], &[SERVERS_TABLE] ; "top_level")]
    #[test_case(STDIO_SERVER, &[SERVER_FIELDS, STDIO_FIELDS], &[] ; "stdio_server")]
    #[test_case(HTTP_SERVER, &[SERVER_FIELDS, HTTP_FIELDS], &[] ; "http_server")]
    #[test_case(OAUTH_CLIENT, &[OAUTH_FIELDS], &[] ; "oauth_client")]
    fn the_reference_describes_every_key(header: &str, fields: &[&[ConfigField]], tables: &[&str]) {
        let config = serde_json::to_value(load_reference(Render::Live { defaults: true })).unwrap();
        let keys: BTreeSet<&str> = config
            .pointer(&json_pointer(header))
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("no [{header}] table"))
            .keys()
            .map(String::as_str)
            .collect();
        let described: BTreeSet<&str> = fields
            .iter()
            .flat_map(|fields| fields.iter().map(|field| field.name))
            .chain(tables.iter().copied())
            .collect();
        assert_eq!(keys, described);
    }
}
