#![forbid(unsafe_code)]

pub mod cli;
pub mod config;
pub mod environment;
mod execution_environment;
pub mod http_policy;
pub mod logging;
pub mod root;
pub mod server;
pub mod transports;

use cli::{CliOptions, Transport};
use server::{ServerBehavior, WorkcellServer};
use transports::{TransportError, TransportOutcome, http::HttpAuthentication};
use workcell_mcp_shell::ShellPermissionPolicy;
use workcell_mcp_web::WebsearchExecutionConfiguration;

pub async fn run(
    options: CliOptions,
    web: WebsearchExecutionConfiguration,
    authentication: Option<HttpAuthentication>,
    shell_policy: ShellPermissionPolicy,
) -> Result<TransportOutcome, Box<dyn std::error::Error>> {
    let root =
        root::resolve_effective_root(options.root.as_deref(), &options.root_relative_subdirectory)?;
    let server = WorkcellServer::configured(
        root.as_deref(),
        options.allow_write,
        web,
        options.web_icons,
        &options.groups,
        ServerBehavior {
            expose_execution_environment: options.expose_execution_environment,
            modern_only: options.modern_only,
        },
        shell_policy,
    )
    .await?;
    match options.transport {
        Transport::Stdio => transports::stdio::run(server).await.map_err(Into::into),
        Transport::Http => transports::http::run(
            server,
            options.port,
            transports::http::HttpConfiguration {
                bind_mode: options.http_bind,
                allowed_hosts: options.allowed_hosts,
                authentication,
            },
        )
        .await
        .map_err(Into::into),
    }
}

pub fn resolve_shell_policy(
    options: &CliOptions,
) -> Result<ShellPermissionPolicy, workcell_mcp_shell::ShellPermissionPolicyError> {
    match options.shell_policy_file.as_deref() {
        Some(path) => ShellPermissionPolicy::from_file(path, options.yolo),
        None if options.yolo => Ok(ShellPermissionPolicy::yolo()),
        None => Ok(ShellPermissionPolicy::restricted()),
    }
}

pub fn validate_http_authentication(
    options: &CliOptions,
    environment_token: Option<String>,
) -> Result<Option<HttpAuthentication>, TransportError> {
    if options.http_token_file.is_some() && environment_token.is_some() {
        return Err(TransportError::HttpAuthentication);
    }
    let token = if let Some(path) = &options.http_token_file {
        let metadata =
            std::fs::symlink_metadata(path).map_err(|_| TransportError::HttpAuthentication)?;
        if !metadata.file_type().is_file() || metadata.len() > 4_098 {
            return Err(TransportError::HttpAuthentication);
        }
        let value =
            std::fs::read_to_string(path).map_err(|_| TransportError::HttpAuthentication)?;
        Some(
            value
                .strip_suffix("\r\n")
                .or_else(|| value.strip_suffix('\n'))
                .unwrap_or(&value)
                .to_owned(),
        )
    } else {
        environment_token
    };
    token.as_deref().map(HttpAuthentication::new).transpose()
}
