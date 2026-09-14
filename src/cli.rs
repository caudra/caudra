use std::{collections::HashSet, fmt, path::PathBuf};

use clap::{Parser, ValueEnum};
// Outbound proxy policy lives in `workcell-net` and is re-exported by the web
// crate, which is the only group that performs egress.
use workcell_mcp_web::ProxyConfiguration;

use crate::environment::StartupEnvironment;
use crate::remote_host::RemoteHostConfiguration;

pub const DEFAULT_PORT: u16 = 3001;
const CODE_WORKER_CACHE_ENV: &str = "WORKCELL_MCP_CODE_WORKER_CACHE";

/// Transfer bounds are independent of `FilesystemLimits::max_write_bytes`. That limit keeps a tool
/// result inside the model's context budget; this one bounds a side channel the model never reads,
/// so the two are sized for different failure modes and must not be unified.
pub const DEFAULT_MAX_TRANSFER_BYTES: usize = 64 * 1024 * 1024;
const MAX_TRANSFER_BYTES_CEILING: usize = 16 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ValueEnum)]
pub enum ToolGroup {
    Files,
    Web,
    Shell,
    // clap derives kebab-case value names, which would spell this `python-execution`. The group
    // selector matches the tool it exposes, so it is named explicitly.
    #[value(name = "python_execution")]
    PythonExecution,
    // Same reason as above: the selector matches the `code_*` tools it exposes.
    #[value(name = "code_graph")]
    CodeGraph,
    Transfer,
}

impl ToolGroup {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Web => "web",
            Self::Shell => "shell",
            Self::PythonExecution => "python_execution",
            Self::CodeGraph => "code_graph",
            Self::Transfer => "transfer",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum Transport {
    #[default]
    Stdio,
    Http,
}

impl Transport {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum HttpBindMode {
    #[default]
    Loopback,
    Container,
}

impl HttpBindMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::Container => "container",
        }
    }
}

#[derive(Parser)]
#[command(
    name = "workcell-mcp",
    version,
    about = "Portable MCP execution server for filesystem, web, and shell tools",
    after_help = "SECURITY: shell commands are not sandboxed by Workcell. Deploy the server inside the container, VM, or host boundary you intend the tools to access."
)]
pub struct RawOptions {
    /// Root exposed to filesystem tools and used as the shell's initial workdir.
    pub root: Option<PathBuf>,

    /// Enable a tool group. Repeat to select multiple groups; defaults to all groups.
    #[arg(long = "tool-group", value_enum, action = clap::ArgAction::Append)]
    pub groups: Vec<ToolGroup>,

    /// Permit file mutations and expose the file mutation tools.
    #[arg(long)]
    pub allow_write: bool,

    /// Resolve and embed source icons in websearch and webfetch results.
    #[arg(long)]
    pub web_icons: bool,

    /// Route web tool egress through this proxy, overriding the environment.
    #[arg(long)]
    pub http_proxy: Option<String>,

    /// Hosts and address blocks that bypass the proxy, overriding NO_PROXY.
    #[arg(long)]
    pub no_proxy: Option<String>,

    /// Ignore any configured or ambient proxy and dial every target directly.
    #[arg(long, conflicts_with_all = ["http_proxy", "no_proxy"])]
    pub no_http_proxy: bool,

    /// Load an immutable shell allow/deny policy from a TOML file.
    #[arg(long)]
    pub shell_policy: Option<PathBuf>,

    /// Permit shell scopes unmatched by policy; explicit deny rules still win.
    #[arg(long)]
    pub yolo: bool,

    /// Return raw command output instead of the filtered shell rendering.
    #[arg(long)]
    pub no_shell_output_filter: bool,

    /// Path to the `monty` worker binary used by the code tool group.
    #[arg(long)]
    pub code_worker: Option<PathBuf>,

    /// Cache directory for an embedded `monty` worker.
    #[arg(long)]
    pub code_worker_cache: Option<PathBuf>,

    /// Skip type checking code snippets before executing them.
    #[arg(long)]
    pub no_code_type_check: bool,

    /// Select an existing relative directory beneath root.
    #[arg(long, default_value = ".")]
    pub root_relative_subdirectory: String,

    /// Load server settings from a dotenv file. Process environment values win.
    #[arg(long)]
    pub env_file: Option<PathBuf>,

    /// MCP transport.
    #[arg(long, value_enum)]
    pub transport: Option<Transport>,

    /// HTTP listen port, including 0 for an ephemeral port.
    #[arg(long)]
    pub port: Option<u16>,

    /// Bind HTTP to loopback or all container interfaces.
    #[arg(long, value_enum)]
    pub http_bind: Option<HttpBindMode>,

    /// Read the HTTP bearer token from this file.
    #[arg(long)]
    pub http_token_file: Option<PathBuf>,

    /// Host authority accepted by HTTP. Repeat for aliases or service DNS names.
    #[arg(long = "allowed-host", action = clap::ArgAction::Append)]
    pub allowed_hosts: Vec<String>,

    /// Disable execution-environment discovery and tool probes.
    #[arg(long)]
    pub no_expose_execution_environment: bool,

    /// Reject all pre-2026 MCP clients instead of serving the stateless fallback.
    #[arg(long)]
    pub modern_only: bool,

    /// Maximum bytes accepted or served by a single `/files` transfer.
    #[arg(long)]
    pub max_transfer_bytes: Option<usize>,

    /// Stable operator identifier for this remote Workcell server.
    #[arg(long)]
    pub remote_server_id: Option<String>,

    /// Stable operator identifier for the workspace containing the configured project.
    #[arg(long)]
    pub remote_workspace_id: Option<String>,

    /// Stable operator-configured generation of the remote workspace.
    #[arg(long)]
    pub remote_workspace_generation: Option<String>,

    /// Stable operator identifier for the one project represented by root.
    #[arg(long)]
    pub remote_root_project_id: Option<String>,

    /// Principal represented by this authenticated HTTP server process.
    #[arg(long)]
    pub remote_principal_id: Option<String>,

    /// Existing private directory used for server-side workspace snapshots.
    #[arg(long)]
    pub snapshot_root: Option<PathBuf>,
}

pub struct CliOptions {
    pub root: Option<PathBuf>,
    pub root_relative_subdirectory: String,
    pub groups: Vec<ToolGroup>,
    pub allow_write: bool,
    pub web_icons: bool,
    pub proxy: ProxyConfiguration,
    pub shell_policy_file: Option<PathBuf>,
    pub yolo: bool,
    pub shell_output_filter: bool,
    pub code_worker: Option<PathBuf>,
    pub code_worker_cache: Option<PathBuf>,
    pub code_type_check: bool,
    pub env_file: Option<PathBuf>,
    pub transport: Transport,
    pub port: u16,
    pub http_bind: HttpBindMode,
    pub http_token_file: Option<PathBuf>,
    pub allowed_hosts: Vec<String>,
    pub expose_execution_environment: bool,
    pub modern_only: bool,
    pub max_transfer_bytes: usize,
    pub remote_host: Option<RemoteHostConfiguration>,
    pub snapshot_root: Option<PathBuf>,
}

impl fmt::Debug for CliOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CliOptions")
            .field("root", &self.root.as_ref().map(|_| "[CONFIGURED]"))
            .field("groups", &self.groups)
            .field("allow_write", &self.allow_write)
            .field("web_icons", &self.web_icons)
            .field(
                "proxy",
                &if self.proxy.is_direct() {
                    "direct"
                } else {
                    "[CONFIGURED]"
                },
            )
            .field(
                "shell_policy_file",
                &self.shell_policy_file.as_ref().map(|_| "[CONFIGURED]"),
            )
            .field("yolo", &self.yolo)
            .field("shell_output_filter", &self.shell_output_filter)
            .field(
                "code_worker",
                &self.code_worker.as_ref().map(|_| "[CONFIGURED]"),
            )
            .field(
                "code_worker_cache",
                &self.code_worker_cache.as_ref().map(|_| "[CONFIGURED]"),
            )
            .field("code_type_check", &self.code_type_check)
            .field("env_file", &self.env_file.as_ref().map(|_| "[CONFIGURED]"))
            .field("transport", &self.transport)
            .field("port", &self.port)
            .field("http_bind", &self.http_bind)
            .field(
                "http_token_file",
                &self.http_token_file.as_ref().map(|_| "[CONFIGURED]"),
            )
            .field("allowed_host_count", &self.allowed_hosts.len())
            .field(
                "expose_execution_environment",
                &self.expose_execution_environment,
            )
            .field("modern_only", &self.modern_only)
            .field("max_transfer_bytes", &self.max_transfer_bytes)
            .field("remote_host_configured", &self.remote_host.is_some())
            .field(
                "snapshot_root",
                &self.snapshot_root.as_ref().map(|_| "[CONFIGURED]"),
            )
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliError {
    InvalidEnvironment,
    InvalidToolGroup,
    DuplicateToolGroup,
    RootRequired,
    RootWithoutLocalTools,
    AllowWriteRequiresFiles,
    WebIconsRequireWeb,
    InvalidProxy,
    ProxyOptionRequiresWeb,
    ShellOptionRequiresShell,
    CodeOptionRequiresPythonExecution,
    HttpOptionRequiresHttp,
    InvalidAllowedHost,
    TransferRequiresHttp,
    TransferOptionRequiresTransfer,
    InvalidMaxTransferBytes,
    InvalidRemoteHost,
    IncompleteRemoteHost,
    RemoteHostRequiresHttp,
    RemoteHostRequiresRoot,
    SnapshotRequiresRemoteHost,
    SnapshotRequiresWrite,
    SnapshotRequiresFiles,
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidEnvironment => "Workcell environment configuration is invalid",
            Self::InvalidToolGroup => {
                "WORKCELL_MCP_TOOL_GROUPS must contain only files, web, shell, python_execution, and transfer"
            }
            Self::DuplicateToolGroup => "each tool group may be selected only once",
            Self::RootRequired => "files, shell, and transfer tools require a root directory",
            Self::RootWithoutLocalTools => {
                "root requires the files, shell, or transfer tool group"
            }
            Self::AllowWriteRequiresFiles => {
                "--allow-write requires the files or transfer tool group"
            }
            Self::WebIconsRequireWeb => "--web-icons requires the web tool group",
            Self::InvalidProxy => {
                "the outbound proxy must be an http or https URL, and any NO_PROXY address block must be valid"
            }
            Self::ProxyOptionRequiresWeb => {
                "--http-proxy, --no-proxy, and --no-http-proxy require the web tool group"
            }
            Self::ShellOptionRequiresShell => {
                "--shell-policy, --yolo, and --no-shell-output-filter require the shell tool group"
            }
            Self::CodeOptionRequiresPythonExecution => {
                "--code-worker, --code-worker-cache, and --no-code-type-check require the python_execution tool group"
            }
            Self::HttpOptionRequiresHttp => "HTTP options require --transport http",
            Self::InvalidAllowedHost => {
                "HTTP allowed hosts must be plain hostnames or IP addresses"
            }
            Self::TransferRequiresHttp => {
                "the transfer tool group moves bytes over HTTP and requires --transport http"
            }
            Self::TransferOptionRequiresTransfer => {
                "--max-transfer-bytes requires the transfer tool group"
            }
            Self::InvalidMaxTransferBytes => {
                "--max-transfer-bytes must be between 1 and 17179869184"
            }
            Self::InvalidRemoteHost => "remote-host identifiers are invalid",
            Self::IncompleteRemoteHost => {
                "remote-host discovery requires server, workspace, workspace-generation, root-project, and principal identifiers"
            }
            Self::RemoteHostRequiresHttp => {
                "remote-host discovery requires --transport http"
            }
            Self::RemoteHostRequiresRoot => "remote-host discovery requires a configured root",
            Self::SnapshotRequiresRemoteHost => {
                "--snapshot-root requires authenticated remote-host discovery"
            }
            Self::SnapshotRequiresWrite => "--snapshot-root requires --allow-write",
            Self::SnapshotRequiresFiles => {
                "--snapshot-root requires the files tool group"
            }
        })
    }
}

impl std::error::Error for CliError {}

impl RawOptions {
    pub fn resolve(self, environment: &StartupEnvironment) -> Result<CliOptions, CliError> {
        let explicit_http_options = self.port.is_some()
            || self.http_bind.is_some()
            || self.http_token_file.is_some()
            || !self.allowed_hosts.is_empty();
        let explicit_shell_options =
            self.shell_policy.is_some() || self.yolo || self.no_shell_output_filter;
        let explicit_code_options = self.code_worker.is_some()
            || self.code_worker_cache.is_some()
            || self.no_code_type_check;
        let max_transfer_bytes_env =
            match environment_value(environment, "WORKCELL_MCP_MAX_TRANSFER_BYTES")? {
                Some(value) => Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| CliError::InvalidMaxTransferBytes)?,
                ),
                None => None,
            };
        let explicit_max_transfer_bytes = max_transfer_bytes_env.is_some();
        // Transfer is deliberately absent from the default set. It only functions under the HTTP
        // transport, so defaulting it on would turn every plain `workcell-mcp <root>` invocation
        // into a startup error.
        let groups = if self.groups.is_empty() {
            match environment_value(environment, "WORKCELL_MCP_TOOL_GROUPS")? {
                Some(value) => parse_groups(&value)?,
                None => vec![
                    ToolGroup::Files,
                    ToolGroup::CodeGraph,
                    ToolGroup::Web,
                    ToolGroup::Shell,
                    ToolGroup::PythonExecution,
                ],
            }
        } else {
            self.groups
        };
        if groups.iter().copied().collect::<HashSet<_>>().len() != groups.len() {
            return Err(CliError::DuplicateToolGroup);
        }
        let transport = self.transport.unwrap_or(
            match environment_value(environment, "WORKCELL_MCP_TRANSPORT")?.as_deref() {
                None | Some("stdio") => Transport::Stdio,
                Some("http") => Transport::Http,
                Some(_) => return Err(CliError::InvalidEnvironment),
            },
        );
        let port = self.port.unwrap_or(
            environment_value(environment, "WORKCELL_MCP_HTTP_PORT")?
                .map(|value| {
                    value
                        .parse::<u16>()
                        .map_err(|_| CliError::InvalidEnvironment)
                })
                .transpose()?
                .unwrap_or(DEFAULT_PORT),
        );
        let http_bind = self.http_bind.unwrap_or(
            match environment_value(environment, "WORKCELL_MCP_HTTP_BIND")?.as_deref() {
                None | Some("loopback") => HttpBindMode::Loopback,
                Some("container") => HttpBindMode::Container,
                Some(_) => return Err(CliError::InvalidEnvironment),
            },
        );
        let http_token_file = self.http_token_file.or(environment_value(
            environment,
            "WORKCELL_MCP_HTTP_TOKEN_FILE",
        )?
        .map(PathBuf::from));
        let allowed_hosts = if self.allowed_hosts.is_empty() {
            environment_value(environment, "WORKCELL_MCP_ALLOWED_HOSTS")?
                .map(|value| value.split(',').map(str::to_owned).collect())
                .unwrap_or_else(|| vec!["127.0.0.1".into(), "localhost".into(), "::1".into()])
        } else {
            self.allowed_hosts
        };
        if allowed_hosts.is_empty() || allowed_hosts.iter().any(|host| !valid_host(host)) {
            return Err(CliError::InvalidAllowedHost);
        }
        let expose_execution_environment = if self.no_expose_execution_environment {
            false
        } else {
            match environment_value(environment, "WORKCELL_MCP_EXPOSE_EXECUTION_ENVIRONMENT")?
                .as_deref()
            {
                None | Some("true") => true,
                Some("false") => false,
                Some(_) => return Err(CliError::InvalidEnvironment),
            }
        };
        let shell_policy_file = self.shell_policy.or(environment_value(
            environment,
            "WORKCELL_MCP_SHELL_POLICY",
        )?
        .map(PathBuf::from));
        let yolo = if self.yolo {
            true
        } else {
            match environment_value(environment, "WORKCELL_MCP_YOLO")?.as_deref() {
                None | Some("false") => false,
                Some("true") => true,
                Some(_) => return Err(CliError::InvalidEnvironment),
            }
        };
        let shell_output_filter = if self.no_shell_output_filter {
            false
        } else {
            match environment_value(environment, "WORKCELL_MCP_SHELL_OUTPUT_FILTER")?.as_deref() {
                None | Some("true") => true,
                Some("false") => false,
                Some(_) => return Err(CliError::InvalidEnvironment),
            }
        };
        let web_icons = if self.web_icons {
            true
        } else {
            match environment_value(environment, "WORKCELL_WEB_ICONS")?.as_deref() {
                None | Some("false") => false,
                Some("true") => true,
                Some(_) => return Err(CliError::InvalidEnvironment),
            }
        };
        let explicit_proxy_options =
            self.http_proxy.is_some() || self.no_proxy.is_some() || self.no_http_proxy;
        let proxy = resolve_proxy(
            environment,
            self.http_proxy.as_deref(),
            self.no_proxy.as_deref(),
            self.no_http_proxy,
        )?;
        let modern_only = if self.modern_only {
            true
        } else {
            match environment_value(environment, "WORKCELL_MCP_MODERN_ONLY")?.as_deref() {
                None | Some("false") => false,
                Some("true") => true,
                Some(_) => return Err(CliError::InvalidEnvironment),
            }
        };
        let code_worker =
            self.code_worker
                .or(environment_value(environment, "WORKCELL_MCP_CODE_WORKER")?.map(PathBuf::from));
        let code_worker_cache =
            self.code_worker_cache
                .or(environment_value(environment, CODE_WORKER_CACHE_ENV)?.map(PathBuf::from));
        // Type checking is on by default: it turns an unsupported API into a diagnostic issued
        // before the snippet runs, which is the difference between one wasted turn and several.
        let code_type_check = if self.no_code_type_check {
            false
        } else {
            match environment_value(environment, "WORKCELL_MCP_CODE_TYPE_CHECK")?.as_deref() {
                None | Some("true") => true,
                Some("false") => false,
                Some(_) => return Err(CliError::InvalidEnvironment),
            }
        };
        let remote_server_id = self.remote_server_id.or(environment_value(
            environment,
            "WORKCELL_MCP_REMOTE_SERVER_ID",
        )?);
        let remote_workspace_id = self.remote_workspace_id.or(environment_value(
            environment,
            "WORKCELL_MCP_REMOTE_WORKSPACE_ID",
        )?);
        let remote_workspace_generation = self.remote_workspace_generation.or(environment_value(
            environment,
            "WORKCELL_MCP_REMOTE_WORKSPACE_GENERATION",
        )?);
        let remote_root_project_id = self.remote_root_project_id.or(environment_value(
            environment,
            "WORKCELL_MCP_REMOTE_ROOT_PROJECT_ID",
        )?);
        let remote_principal_id = self.remote_principal_id.or(environment_value(
            environment,
            "WORKCELL_MCP_REMOTE_PRINCIPAL_ID",
        )?);
        let remote_host = match (
            remote_server_id,
            remote_workspace_id,
            remote_workspace_generation,
            remote_root_project_id,
            remote_principal_id,
        ) {
            (None, None, None, None, None) => None,
            (Some(server), Some(workspace), Some(generation), Some(project), Some(principal)) => {
                Some(
                    RemoteHostConfiguration::new(server, workspace, generation, project, principal)
                        .map_err(|_| CliError::InvalidRemoteHost)?,
                )
            }
            _ => return Err(CliError::IncompleteRemoteHost),
        };
        if remote_host.is_some() && transport != Transport::Http {
            return Err(CliError::RemoteHostRequiresHttp);
        }
        if remote_host.is_some() && self.root.is_none() {
            return Err(CliError::RemoteHostRequiresRoot);
        }
        let snapshot_root =
            self.snapshot_root.or(
                environment_value(environment, "WORKCELL_MCP_SNAPSHOT_ROOT")?.map(PathBuf::from),
            );
        if snapshot_root.is_some() && remote_host.is_none() {
            return Err(CliError::SnapshotRequiresRemoteHost);
        }
        if snapshot_root.is_some() && !self.allow_write {
            return Err(CliError::SnapshotRequiresWrite);
        }
        if snapshot_root.is_some() && !groups.contains(&ToolGroup::Files) {
            return Err(CliError::SnapshotRequiresFiles);
        }

        // Transfer and code_graph both resolve every path through a confined `FileToolGroup`, so
        // they need a root for the same reason the files tools do.
        let has_local = groups.contains(&ToolGroup::Files)
            || groups.contains(&ToolGroup::Shell)
            || groups.contains(&ToolGroup::CodeGraph)
            || groups.contains(&ToolGroup::Transfer)
            || remote_host.is_some();
        if has_local && self.root.is_none() {
            return Err(CliError::RootRequired);
        }
        if !has_local && self.root.is_some() {
            return Err(CliError::RootWithoutLocalTools);
        }
        if self.allow_write
            && !(groups.contains(&ToolGroup::Files) || groups.contains(&ToolGroup::Transfer))
        {
            return Err(CliError::AllowWriteRequiresFiles);
        }
        if web_icons && !groups.contains(&ToolGroup::Web) {
            return Err(CliError::WebIconsRequireWeb);
        }
        // Ambient proxy variables belong to the whole environment, so they are
        // only ignored without the web group. An explicit flag is a statement
        // about this process and is an error when nothing would honor it.
        if explicit_proxy_options && !groups.contains(&ToolGroup::Web) {
            return Err(CliError::ProxyOptionRequiresWeb);
        }
        if !groups.contains(&ToolGroup::Shell)
            && (explicit_shell_options || shell_policy_file.is_some() || yolo)
        {
            return Err(CliError::ShellOptionRequiresShell);
        }
        if !groups.contains(&ToolGroup::PythonExecution)
            && (explicit_code_options
                || code_worker.is_some()
                || code_worker_cache.is_some()
                || !code_type_check)
        {
            return Err(CliError::CodeOptionRequiresPythonExecution);
        }
        if transport == Transport::Stdio && (explicit_http_options || http_token_file.is_some()) {
            return Err(CliError::HttpOptionRequiresHttp);
        }
        let has_transfer = groups.contains(&ToolGroup::Transfer);
        if has_transfer && transport != Transport::Http {
            return Err(CliError::TransferRequiresHttp);
        }
        if !has_transfer && (self.max_transfer_bytes.is_some() || explicit_max_transfer_bytes) {
            return Err(CliError::TransferOptionRequiresTransfer);
        }
        let max_transfer_bytes = match self.max_transfer_bytes.or(max_transfer_bytes_env) {
            Some(value) if value == 0 || value > MAX_TRANSFER_BYTES_CEILING => {
                return Err(CliError::InvalidMaxTransferBytes);
            }
            Some(value) => value,
            None => DEFAULT_MAX_TRANSFER_BYTES,
        };

        Ok(CliOptions {
            root: self.root,
            root_relative_subdirectory: self.root_relative_subdirectory,
            groups,
            allow_write: self.allow_write,
            web_icons,
            proxy,
            shell_policy_file,
            yolo,
            shell_output_filter,
            code_worker,
            code_worker_cache: code_worker_cache.or_else(default_code_worker_cache),
            code_type_check,
            env_file: self.env_file,
            transport,
            port,
            http_bind,
            http_token_file,
            allowed_hosts,
            expose_execution_environment,
            modern_only,
            max_transfer_bytes,
            remote_host,
            snapshot_root,
        })
    }
}

/// Resolve the outbound proxy for web tools.
///
/// Conventional variables are honored so a sandbox that already exports them to
/// every guest process does not need Workcell-specific wiring. They are read
/// exactly once, here: the shell tool changes only its children's environment,
/// never this process's, so the selection cannot be influenced at runtime.
///
/// A malformed value is a startup error rather than a fall back to direct. Under
/// an enforcing sandbox, silently dialling around the proxy is the one outcome
/// that looks like an egress bypass.
fn resolve_proxy(
    environment: &StartupEnvironment,
    explicit: Option<&str>,
    explicit_bypass: Option<&str>,
    disabled: bool,
) -> Result<ProxyConfiguration, CliError> {
    resolve_proxy_with(
        |name| environment_value(environment, name),
        explicit,
        explicit_bypass,
        disabled,
    )
}

fn resolve_proxy_with<F>(
    mut read: F,
    explicit: Option<&str>,
    explicit_bypass: Option<&str>,
    disabled: bool,
) -> Result<ProxyConfiguration, CliError>
where
    F: FnMut(&str) -> Result<Option<String>, CliError>,
{
    if disabled {
        return Ok(ProxyConfiguration::direct());
    }
    let bypass = match explicit_bypass {
        Some(value) => Some(value.to_owned()),
        None => first_value(
            &mut read,
            &["WORKCELL_MCP_NO_PROXY", "NO_PROXY", "no_proxy"],
        )?,
    };
    let configured = match explicit {
        Some(value) => Some(value.to_owned()),
        None => first_value(&mut read, &["WORKCELL_MCP_HTTP_PROXY"])?,
    };
    if let Some(value) = configured {
        return ProxyConfiguration::from_values(None, None, Some(&value), bypass.as_deref())
            .map_err(|_| CliError::InvalidProxy);
    }
    // Per-scheme values win over the catch-all, and uppercase wins over lower.
    let all = first_value(&mut read, &["ALL_PROXY", "all_proxy"])?;
    let http = first_value(&mut read, &["HTTP_PROXY", "http_proxy"])?;
    let https = first_value(&mut read, &["HTTPS_PROXY", "https_proxy"])?;
    ProxyConfiguration::from_values(
        http.as_deref(),
        https.as_deref(),
        all.as_deref(),
        bypass.as_deref(),
    )
    .map_err(|_| CliError::InvalidProxy)
}

fn first_value<F>(read: &mut F, names: &[&str]) -> Result<Option<String>, CliError>
where
    F: FnMut(&str) -> Result<Option<String>, CliError>,
{
    for name in names {
        if let Some(value) = read(name)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn default_code_worker_cache() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(path) = environment_path("LOCALAPPDATA") {
        return Some(PathBuf::from(path).join("workcell-mcp"));
    }
    #[cfg(not(windows))]
    if let Some(path) = environment_path("XDG_CACHE_HOME") {
        return Some(PathBuf::from(path).join("workcell-mcp"));
    }
    environment_path("HOME").map(|path| PathBuf::from(path).join(".cache/workcell-mcp"))
}

fn environment_path(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn environment_value(
    environment: &StartupEnvironment,
    name: &str,
) -> Result<Option<String>, CliError> {
    environment
        .read(name)
        .map_err(|_| CliError::InvalidEnvironment)
}

fn parse_groups(value: &str) -> Result<Vec<ToolGroup>, CliError> {
    if value.is_empty() || value.trim() != value {
        return Err(CliError::InvalidToolGroup);
    }
    value
        .split(',')
        .map(|group| match group {
            "files" => Ok(ToolGroup::Files),
            "web" => Ok(ToolGroup::Web),
            "shell" => Ok(ToolGroup::Shell),
            "python_execution" => Ok(ToolGroup::PythonExecution),
            "code_graph" => Ok(ToolGroup::CodeGraph),
            "transfer" => Ok(ToolGroup::Transfer),
            _ => Err(CliError::InvalidToolGroup),
        })
        .collect()
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.trim() == host
        && !host.contains(['/', '@', ' '])
        && !host.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_groups_strictly() {
        assert_eq!(
            parse_groups("files,web,shell").unwrap(),
            [ToolGroup::Files, ToolGroup::Web, ToolGroup::Shell]
        );
        // Every selector round-trips through the name the group reports for itself, so an
        // operator can always paste `--tools` back from a disclosure.
        for group in [
            ToolGroup::Files,
            ToolGroup::Web,
            ToolGroup::Shell,
            ToolGroup::PythonExecution,
            ToolGroup::CodeGraph,
            ToolGroup::Transfer,
        ] {
            assert_eq!(parse_groups(group.as_str()).unwrap(), [group]);
        }
        assert_eq!(
            parse_groups("unknown").unwrap_err(),
            CliError::InvalidToolGroup
        );
        assert_eq!(
            parse_groups("files, files").unwrap_err(),
            CliError::InvalidToolGroup
        );
    }

    #[test]
    fn allowed_hosts_exclude_urls_and_userinfo() {
        for host in ["127.0.0.1", "localhost", "::1", "workcell.internal"] {
            assert!(valid_host(host));
        }
        for host in ["", "https://example.com", "user@example.com", "bad host"] {
            assert!(!valid_host(host));
        }
    }

    #[test]
    fn remote_host_configuration_is_complete_http_only_and_rooted() {
        let raw = RawOptions::try_parse_from([
            "workcell-mcp",
            "--transport",
            "http",
            "--tool-group",
            "files",
            "--remote-server-id",
            "server",
            "--remote-workspace-id",
            "workspace",
            "--remote-workspace-generation",
            "generation",
            "--remote-root-project-id",
            "project",
            "--remote-principal-id",
            "principal",
            ".",
        ])
        .unwrap();
        let environment = StartupEnvironment::load(None).unwrap();
        let options = raw.resolve(&environment).unwrap();
        assert_eq!(
            options.remote_host.unwrap().workspace_generation.as_str(),
            "generation"
        );

        let partial = RawOptions::try_parse_from([
            "workcell-mcp",
            "--transport",
            "http",
            "--remote-server-id",
            "server",
            ".",
        ])
        .unwrap();
        assert_eq!(
            partial.resolve(&environment).unwrap_err(),
            CliError::IncompleteRemoteHost
        );

        let missing_generation = RawOptions::try_parse_from([
            "workcell-mcp",
            "--transport",
            "http",
            "--remote-server-id",
            "server",
            "--remote-workspace-id",
            "workspace",
            "--remote-root-project-id",
            "project",
            "--remote-principal-id",
            "principal",
            ".",
        ])
        .unwrap();
        assert_eq!(
            missing_generation.resolve(&environment).unwrap_err(),
            CliError::IncompleteRemoteHost
        );

        let stdio = RawOptions::try_parse_from([
            "workcell-mcp",
            "--remote-server-id",
            "server",
            "--remote-workspace-id",
            "workspace",
            "--remote-workspace-generation",
            "generation",
            "--remote-root-project-id",
            "project",
            "--remote-principal-id",
            "principal",
            ".",
        ])
        .unwrap();
        assert_eq!(
            stdio.resolve(&environment).unwrap_err(),
            CliError::RemoteHostRequiresHttp
        );

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("remote.env");
        std::fs::write(
            &path,
            "WORKCELL_MCP_TRANSPORT=http\nWORKCELL_MCP_REMOTE_SERVER_ID=server-env\nWORKCELL_MCP_REMOTE_WORKSPACE_ID=workspace-env\nWORKCELL_MCP_REMOTE_WORKSPACE_GENERATION=generation-env\nWORKCELL_MCP_REMOTE_ROOT_PROJECT_ID=project-env\nWORKCELL_MCP_REMOTE_PRINCIPAL_ID=principal-env\n",
        )
        .unwrap();
        let environment = StartupEnvironment::load(Some(&path)).unwrap();
        let configured = RawOptions::try_parse_from(["workcell-mcp", "."])
            .unwrap()
            .resolve(&environment)
            .unwrap();
        assert_eq!(
            configured
                .remote_host
                .unwrap()
                .workspace_generation
                .as_str(),
            "generation-env"
        );
    }

    #[test]
    fn snapshot_storage_is_explicit_remote_write_configuration() {
        let environment = StartupEnvironment::load(None).unwrap();
        let without_remote = RawOptions::try_parse_from([
            "workcell-mcp",
            "--transport",
            "http",
            "--tool-group",
            "files",
            "--allow-write",
            "--snapshot-root",
            "/private/snapshots",
            ".",
        ])
        .unwrap();
        assert_eq!(
            without_remote.resolve(&environment).unwrap_err(),
            CliError::SnapshotRequiresRemoteHost
        );

        let configured = RawOptions::try_parse_from([
            "workcell-mcp",
            "--transport",
            "http",
            "--tool-group",
            "files",
            "--allow-write",
            "--remote-server-id",
            "server",
            "--remote-workspace-id",
            "workspace",
            "--remote-workspace-generation",
            "generation",
            "--remote-root-project-id",
            "project",
            "--remote-principal-id",
            "principal",
            "--snapshot-root",
            "/private/snapshots",
            ".",
        ])
        .unwrap()
        .resolve(&environment)
        .unwrap();
        assert_eq!(
            configured.snapshot_root.as_deref(),
            Some(std::path::Path::new("/private/snapshots"))
        );
        assert!(!format!("{configured:?}").contains("/private/snapshots"));
    }

    /// Transfer mints URLs for an HTTP route this process would never serve over stdio, so the
    /// combination is a startup error rather than a group that silently hands out dead URLs.
    #[test]
    fn transfer_requires_the_http_transport() {
        let raw = RawOptions::try_parse_from([
            "workcell-mcp",
            "--tool-group",
            "files",
            "--tool-group",
            "transfer",
            ".",
        ])
        .unwrap();
        let environment = StartupEnvironment::load(None).unwrap();
        assert_eq!(
            raw.resolve(&environment).unwrap_err(),
            CliError::TransferRequiresHttp
        );
    }

    #[test]
    fn transfer_options_require_the_transfer_group() {
        let raw = RawOptions::try_parse_from([
            "workcell-mcp",
            "--tool-group",
            "files",
            "--max-transfer-bytes",
            "1024",
            ".",
        ])
        .unwrap();
        let environment = StartupEnvironment::load(None).unwrap();
        assert_eq!(
            raw.resolve(&environment).unwrap_err(),
            CliError::TransferOptionRequiresTransfer
        );
    }

    #[test]
    fn transfer_over_http_resolves_with_a_bounded_limit() {
        for (argument, expected) in [
            (None, Ok(DEFAULT_MAX_TRANSFER_BYTES)),
            (Some("0"), Err(CliError::InvalidMaxTransferBytes)),
            (Some("1048576"), Ok(1_048_576)),
            (Some("17179869184"), Ok(MAX_TRANSFER_BYTES_CEILING)),
            (Some("17179869185"), Err(CliError::InvalidMaxTransferBytes)),
        ] {
            let mut arguments = vec![
                "workcell-mcp",
                "--tool-group",
                "files",
                "--tool-group",
                "transfer",
                "--transport",
                "http",
            ];
            if let Some(argument) = argument {
                arguments.extend(["--max-transfer-bytes", argument]);
            }
            arguments.push(".");
            let raw = RawOptions::try_parse_from(arguments).unwrap();
            let environment = StartupEnvironment::load(None).unwrap();
            let actual = raw
                .resolve(&environment)
                .map(|options| options.max_transfer_bytes);
            assert_eq!(actual, expected, "{argument:?}");
        }
    }

    #[test]
    fn shell_policy_options_require_the_shell_group() {
        let raw =
            RawOptions::try_parse_from(["workcell-mcp", "--tool-group", "web", "--yolo"]).unwrap();
        let environment = StartupEnvironment::load(None).unwrap();
        assert_eq!(
            raw.resolve(&environment).unwrap_err(),
            CliError::ShellOptionRequiresShell
        );
    }

    #[test]
    fn shell_output_filtering_is_on_by_default_and_opt_out() {
        let environment = StartupEnvironment::load(None).unwrap();
        let default = RawOptions::try_parse_from(["workcell-mcp", "/"])
            .unwrap()
            .resolve(&environment)
            .unwrap();
        assert!(default.shell_output_filter);

        let disabled =
            RawOptions::try_parse_from(["workcell-mcp", "--no-shell-output-filter", "/"])
                .unwrap()
                .resolve(&environment)
                .unwrap();
        assert!(!disabled.shell_output_filter);
    }

    #[test]
    fn shell_output_filter_opt_out_requires_the_shell_group() {
        let raw = RawOptions::try_parse_from([
            "workcell-mcp",
            "--tool-group",
            "web",
            "--no-shell-output-filter",
        ])
        .unwrap();
        let environment = StartupEnvironment::load(None).unwrap();
        assert_eq!(
            raw.resolve(&environment).unwrap_err(),
            CliError::ShellOptionRequiresShell
        );
    }

    #[test]
    fn web_icons_require_the_web_group() {
        let raw = RawOptions::try_parse_from([
            "workcell-mcp",
            "--tool-group",
            "files",
            "--web-icons",
            ".",
        ])
        .unwrap();
        let environment = StartupEnvironment::load(None).unwrap();
        assert_eq!(
            raw.resolve(&environment).unwrap_err(),
            CliError::WebIconsRequireWeb
        );

        let raw = RawOptions::try_parse_from(["workcell-mcp", "--tool-group", "web"]).unwrap();
        assert!(!raw.resolve(&environment).unwrap().web_icons);

        let raw =
            RawOptions::try_parse_from(["workcell-mcp", "--tool-group", "web", "--web-icons"])
                .unwrap();
        assert!(raw.resolve(&environment).unwrap().web_icons);
    }

    fn proxy_from(values: &[(&str, &str)]) -> Result<ProxyConfiguration, CliError> {
        resolve_proxy_with(
            |name| {
                Ok(values
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned()))
            },
            None,
            None,
            false,
        )
    }

    fn proxies(configuration: &ProxyConfiguration, url: &str) -> bool {
        !matches!(
            configuration.route(&url::Url::parse(url).unwrap()),
            workcell_mcp_web::ProxyRoute::Direct
        )
    }

    /// A sandbox exports the conventional variables to every guest process, so
    /// honoring them is what makes the web tools work under egress enforcement.
    #[test]
    fn conventional_environment_variables_configure_the_proxy() {
        let configuration = proxy_from(&[("HTTPS_PROXY", "http://proxy.internal:8080")]).unwrap();
        assert!(proxies(&configuration, "https://example.com/"));
        assert!(!proxies(&configuration, "http://example.com/"));

        let configuration = proxy_from(&[("ALL_PROXY", "http://proxy.internal:8080")]).unwrap();
        assert!(proxies(&configuration, "https://example.com/"));
        assert!(proxies(&configuration, "http://example.com/"));

        let configuration = proxy_from(&[("https_proxy", "http://proxy.internal:8080")]).unwrap();
        assert!(proxies(&configuration, "https://example.com/"));

        assert!(proxy_from(&[]).unwrap().is_direct());
    }

    #[test]
    fn proxy_resolution_follows_a_fixed_precedence() {
        let bypassed = proxy_from(&[
            ("HTTPS_PROXY", "http://ambient.internal:8080"),
            ("NO_PROXY", "internal.example.com"),
        ])
        .unwrap();
        assert!(!proxies(&bypassed, "https://internal.example.com/"));
        assert!(proxies(&bypassed, "https://example.com/"));

        // A Workcell-specific value overrides the ambient environment entirely.
        let workcell = proxy_from(&[
            ("HTTPS_PROXY", "http://ambient.internal:8080"),
            ("WORKCELL_MCP_HTTP_PROXY", "http://chosen.internal:3128"),
            ("NO_PROXY", "example.com"),
            ("WORKCELL_MCP_NO_PROXY", "other.example.com"),
        ])
        .unwrap();
        assert!(proxies(&workcell, "https://example.com/"));
        assert!(!proxies(&workcell, "https://other.example.com/"));

        // An explicit flag outranks every variable, and the opt-out outranks it.
        let flag = resolve_proxy_with(
            |_| Ok(Some("http://ambient.internal:8080".to_owned())),
            Some("http://flag.internal:3128"),
            Some("flagged.example.com"),
            false,
        )
        .unwrap();
        assert!(!proxies(&flag, "https://flagged.example.com/"));
        assert!(proxies(&flag, "https://example.com/"));

        let disabled = resolve_proxy_with(
            |_| Ok(Some("http://ambient.internal:8080".to_owned())),
            None,
            None,
            true,
        )
        .unwrap();
        assert!(disabled.is_direct());
    }

    /// Falling back to a direct dial would look like an egress bypass attempt
    /// to an enforcing sandbox, so an unusable value stops startup instead.
    #[test]
    fn an_unusable_proxy_value_fails_startup() {
        for values in [
            [("HTTPS_PROXY", "socks5://proxy.internal:1080")],
            [("ALL_PROXY", "not a url")],
            [("WORKCELL_MCP_HTTP_PROXY", "ftp://proxy.internal")],
        ] {
            assert_eq!(proxy_from(&values).unwrap_err(), CliError::InvalidProxy);
        }
        assert_eq!(
            proxy_from(&[
                ("HTTPS_PROXY", "http://proxy.internal:8080"),
                ("NO_PROXY", "10.0.0.0/99"),
            ])
            .unwrap_err(),
            CliError::InvalidProxy
        );
    }

    #[test]
    fn proxy_flags_require_the_web_group_and_stay_redacted() {
        let environment = StartupEnvironment::load(None).unwrap();
        let raw = RawOptions::try_parse_from([
            "workcell-mcp",
            "--tool-group",
            "files",
            "--http-proxy",
            "http://proxy.internal:8080",
            ".",
        ])
        .unwrap();
        assert_eq!(
            raw.resolve(&environment).unwrap_err(),
            CliError::ProxyOptionRequiresWeb
        );

        let raw = RawOptions::try_parse_from([
            "workcell-mcp",
            "--tool-group",
            "web",
            "--http-proxy",
            "http://operator:hunter2@proxy.internal:8080",
        ])
        .unwrap();
        let options = raw.resolve(&environment).unwrap();
        assert!(!options.proxy.is_direct());
        let rendered = format!("{options:?}");
        assert!(rendered.contains("proxy: \"[CONFIGURED]\""));
        assert!(!rendered.contains("hunter2"));
        assert!(!rendered.contains("proxy.internal"));
    }

    #[test]
    fn parses_modern_only_flag() {
        let raw = RawOptions::try_parse_from(["workcell-mcp", "--tool-group", "web"]).unwrap();
        assert!(!raw.modern_only);

        let raw =
            RawOptions::try_parse_from(["workcell-mcp", "--tool-group", "web", "--modern-only"])
                .unwrap();
        assert!(raw.modern_only);
    }

    #[test]
    fn code_worker_cache_requires_python_and_is_redacted() {
        let environment = StartupEnvironment::load(None).unwrap();
        let without_code = RawOptions::try_parse_from([
            "workcell-mcp",
            "--tool-group",
            "web",
            "--code-worker-cache",
            "/private/cache",
        ])
        .unwrap();
        assert_eq!(
            without_code.resolve(&environment).unwrap_err(),
            CliError::CodeOptionRequiresPythonExecution
        );

        let with_code = RawOptions::try_parse_from([
            "workcell-mcp",
            "--tool-group",
            "python_execution",
            "--code-worker-cache",
            "/private/cache",
        ])
        .unwrap()
        .resolve(&environment)
        .unwrap();
        assert_eq!(
            with_code.code_worker_cache,
            Some(PathBuf::from("/private/cache"))
        );
        assert!(!format!("{with_code:?}").contains("/private/cache"));
    }

    #[test]
    fn code_worker_cache_environment_requires_python() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("server.env");
        std::fs::write(&path, "WORKCELL_MCP_CODE_WORKER_CACHE=/private/cache\n")
            .expect("write environment");
        let environment = StartupEnvironment::load(Some(&path)).expect("load environment");
        let raw = RawOptions::try_parse_from(["workcell-mcp", "--tool-group", "web"]).unwrap();

        assert_eq!(
            raw.resolve(&environment).unwrap_err(),
            CliError::CodeOptionRequiresPythonExecution
        );
    }
}
