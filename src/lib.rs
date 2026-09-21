#![forbid(unsafe_code)]

pub mod cli;
pub mod config;
pub mod environment;
pub use workcell_environment as execution_environment;
pub mod http_policy;
pub mod logging;
pub mod remote_host;
pub mod root;
pub mod server;
pub mod transfer;
pub mod transports;

use cli::{CliOptions, Transport};
use server::{ServerBehavior, ToolConfiguration, WorkcellServer};
use std::path::{Path, PathBuf};
use transports::{TransportError, TransportOutcome, http::HttpAuthentication};
use workcell_mcp_code::{CodeConfiguration, WorkerSource};
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
    let snapshot_exclusions =
        cache_snapshot_exclusion(root.as_deref(), options.code_worker_cache.as_deref())?;
    let server = WorkcellServer::configured(
        root.as_deref(),
        &options.groups,
        ServerBehavior {
            expose_execution_environment: options.expose_execution_environment,
            modern_only: options.modern_only,
        },
        ToolConfiguration {
            allow_write: options.allow_write,
            web,
            web_icons: options.web_icons,
            proxy: options.proxy,
            shell_policy,
            shell_output_filter: options.shell_output_filter,
            honor_gitignore: options.honor_gitignore,
            code: CodeConfiguration {
                worker: options.code_worker.as_deref().map_or(
                    WorkerSource::Discover {
                        bundled_cache_root: options.code_worker_cache.as_deref(),
                    },
                    WorkerSource::Path,
                ),
                type_check: options.code_type_check,
            },
            max_transfer_bytes: options.max_transfer_bytes,
            snapshot_root: options.snapshot_root.as_deref(),
            transfer_root: options.transfer_root.as_deref(),
            snapshot_exclusions: snapshot_exclusions.as_slice(),
        },
    )
    .await?;
    let outcome = match options.transport {
        Transport::Stdio => transports::stdio::run(server.clone())
            .await
            .map_err(Into::into),
        Transport::Http => transports::http::run(
            server.clone(),
            options.port,
            transports::http::HttpConfiguration {
                bind_mode: options.http_bind,
                allowed_hosts: options.allowed_hosts,
                authentication,
                remote_host: options.remote_host,
            },
        )
        .await
        .map_err(Into::into),
    };
    // Ask pooled workers to exit cleanly. Dropping the server would kill them, which is equally
    // safe but leaves a SIGKILL in the operator's logs for an ordinary shutdown.
    server.shutdown().await;
    outcome
}

fn cache_snapshot_exclusion(
    root: Option<&Path>,
    cache: Option<&Path>,
) -> std::io::Result<Option<PathBuf>> {
    let (Some(root), Some(cache)) = (root, cache) else {
        return Ok(None);
    };
    let cache = if cache.is_absolute() {
        cache.to_path_buf()
    } else {
        std::env::current_dir()?.join(cache)
    };
    Ok(cache.starts_with(root).then_some(cache))
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

#[cfg(test)]
mod tests {
    use super::cache_snapshot_exclusion;
    use std::path::Path;

    #[test]
    fn worker_cache_outside_workspace_is_not_a_snapshot_exclusion() {
        let root = Path::new("/workspace");
        assert_eq!(
            cache_snapshot_exclusion(Some(root), Some(Path::new("/home/cache"))).unwrap(),
            None
        );
        assert_eq!(
            cache_snapshot_exclusion(Some(root), Some(Path::new("/workspace/cache"))).unwrap(),
            Some(root.join("cache"))
        );
        assert_eq!(cache_snapshot_exclusion(Some(root), None).unwrap(), None);
    }
}
