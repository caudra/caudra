use std::{collections::HashSet, fmt, path::PathBuf};

use clap::{Parser, ValueEnum};

use crate::environment::StartupEnvironment;

pub const DEFAULT_PORT: u16 = 3001;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ValueEnum)]
pub enum ToolGroup {
    Files,
    Web,
    Shell,
}

impl ToolGroup {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Web => "web",
            Self::Shell => "shell",
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

    /// Permit non-dry-run file mutations.
    #[arg(long)]
    pub allow_write: bool,

    /// Resolve and embed source icons in websearch and webfetch results.
    #[arg(long)]
    pub web_icons: bool,

    /// Load an immutable shell allow/deny policy from a TOML file.
    #[arg(long)]
    pub shell_policy: Option<PathBuf>,

    /// Permit shell scopes unmatched by policy; explicit deny rules still win.
    #[arg(long)]
    pub yolo: bool,

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
}

pub struct CliOptions {
    pub root: Option<PathBuf>,
    pub root_relative_subdirectory: String,
    pub groups: Vec<ToolGroup>,
    pub allow_write: bool,
    pub web_icons: bool,
    pub shell_policy_file: Option<PathBuf>,
    pub yolo: bool,
    pub env_file: Option<PathBuf>,
    pub transport: Transport,
    pub port: u16,
    pub http_bind: HttpBindMode,
    pub http_token_file: Option<PathBuf>,
    pub allowed_hosts: Vec<String>,
    pub expose_execution_environment: bool,
    pub modern_only: bool,
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
                "shell_policy_file",
                &self.shell_policy_file.as_ref().map(|_| "[CONFIGURED]"),
            )
            .field("yolo", &self.yolo)
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
    ShellOptionRequiresShell,
    HttpOptionRequiresHttp,
    InvalidAllowedHost,
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidEnvironment => "Workcell environment configuration is invalid",
            Self::InvalidToolGroup => {
                "WORKCELL_MCP_TOOL_GROUPS must contain only files, web, and shell"
            }
            Self::DuplicateToolGroup => "each tool group may be selected only once",
            Self::RootRequired => "files and shell tools require a root directory",
            Self::RootWithoutLocalTools => "root requires the files or shell tool group",
            Self::AllowWriteRequiresFiles => "--allow-write requires the files tool group",
            Self::WebIconsRequireWeb => "--web-icons requires the web tool group",
            Self::ShellOptionRequiresShell => {
                "--shell-policy and --yolo require the shell tool group"
            }
            Self::HttpOptionRequiresHttp => "HTTP options require --transport http",
            Self::InvalidAllowedHost => {
                "HTTP allowed hosts must be plain hostnames or IP addresses"
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
        let explicit_shell_options = self.shell_policy.is_some() || self.yolo;
        let groups = if self.groups.is_empty() {
            match environment_value(environment, "WORKCELL_MCP_TOOL_GROUPS")? {
                Some(value) => parse_groups(&value)?,
                None => vec![ToolGroup::Files, ToolGroup::Web, ToolGroup::Shell],
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
        let web_icons = if self.web_icons {
            true
        } else {
            match environment_value(environment, "WORKCELL_WEB_ICONS")?.as_deref() {
                None | Some("false") => false,
                Some("true") => true,
                Some(_) => return Err(CliError::InvalidEnvironment),
            }
        };
        let modern_only = if self.modern_only {
            true
        } else {
            match environment_value(environment, "WORKCELL_MCP_MODERN_ONLY")?.as_deref() {
                None | Some("false") => false,
                Some("true") => true,
                Some(_) => return Err(CliError::InvalidEnvironment),
            }
        };

        let has_local = groups.contains(&ToolGroup::Files) || groups.contains(&ToolGroup::Shell);
        if has_local && self.root.is_none() {
            return Err(CliError::RootRequired);
        }
        if !has_local && self.root.is_some() {
            return Err(CliError::RootWithoutLocalTools);
        }
        if self.allow_write && !groups.contains(&ToolGroup::Files) {
            return Err(CliError::AllowWriteRequiresFiles);
        }
        if web_icons && !groups.contains(&ToolGroup::Web) {
            return Err(CliError::WebIconsRequireWeb);
        }
        if !groups.contains(&ToolGroup::Shell)
            && (explicit_shell_options || shell_policy_file.is_some() || yolo)
        {
            return Err(CliError::ShellOptionRequiresShell);
        }
        if transport == Transport::Stdio && (explicit_http_options || http_token_file.is_some()) {
            return Err(CliError::HttpOptionRequiresHttp);
        }

        Ok(CliOptions {
            root: self.root,
            root_relative_subdirectory: self.root_relative_subdirectory,
            groups,
            allow_write: self.allow_write,
            web_icons,
            shell_policy_file,
            yolo,
            env_file: self.env_file,
            transport,
            port,
            http_bind,
            http_token_file,
            allowed_hosts,
            expose_execution_environment,
            modern_only,
        })
    }
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

    #[test]
    fn parses_modern_only_flag() {
        let raw = RawOptions::try_parse_from(["workcell-mcp", "--tool-group", "web"]).unwrap();
        assert!(!raw.modern_only);

        let raw =
            RawOptions::try_parse_from(["workcell-mcp", "--tool-group", "web", "--modern-only"])
                .unwrap();
        assert!(raw.modern_only);
    }
}
