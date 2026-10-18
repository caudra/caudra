use std::collections::HashMap;
use std::iter;
use std::path::Path;

use agent_client_protocol_schema::v1::{Error as AcpError, McpServer};
use caudra_agent::mcp::config::{McpServerStatus, RawHttpFields, RawStdioFields, RawTransport};
use caudra_agent::mcp::{self, McpHandle};
use caudra_config::MAX_SERVER_NAME_LEN;
use tracing::warn;

use crate::errors::invalid_params;

const REMOTE_SESSION_SERVERS: &str =
    "session MCP servers are disabled for remote Workcell sessions";

/// Servers the client injects on `session/new` and `session/load`. A transport we
/// cannot speak is dropped like a broken `mcp.toml` entry: losing one server beats
/// losing the session.
fn injected_servers(servers: &[McpServer]) -> Vec<(String, RawTransport)> {
    servers
        .iter()
        .filter_map(|server| match server {
            McpServer::Http(http) => Some((
                server_name(&http.name),
                RawTransport::Http(RawHttpFields {
                    url: http.url.clone(),
                    headers: pairs(&http.headers, |h| (&h.name, &h.value)),
                    oauth: None,
                }),
            )),
            McpServer::Stdio(stdio) => Some((
                server_name(&stdio.name),
                RawTransport::Stdio(RawStdioFields {
                    command: iter::once(stdio.command.to_string_lossy().into_owned())
                        .chain(stdio.args.iter().cloned())
                        .collect(),
                    environment: pairs(&stdio.env, |e| (&e.name, &e.value)),
                }),
            )),
            _ => {
                warn!("ignoring injected MCP server, only http and stdio are supported");
                None
            }
        })
        .collect()
}

/// Clients name their servers freely, caudra names them like `mcp.toml` does.
fn server_name(name: &str) -> String {
    name.chars()
        .take(MAX_SERVER_NAME_LEN)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn pairs<T>(items: &[T], split: impl Fn(&T) -> (&String, &String)) -> HashMap<String, String> {
    items
        .iter()
        .map(|item| {
            let (name, value) = split(item);
            (name.clone(), value.clone())
        })
        .collect()
}

/// MCP is per session: the client picks the cwd and may inject its own servers.
/// Returns as soon as the config is read, the first prompt waits for the tools.
pub(crate) async fn start_mcp(
    cwd: &Path,
    servers: &[McpServer],
    remote: bool,
) -> Option<McpHandle> {
    let (handle, errors) = if remote {
        mcp::start_global(cwd).await
    } else {
        mcp::start_with_extra(cwd, injected_servers(servers)).await
    };
    if !errors.is_empty() {
        warn!(%errors, "MCP config errors");
    }
    if let Some(handle) = &handle {
        let awaiting: Vec<_> = handle
            .reader()
            .load()
            .infos
            .iter()
            .filter(|info| info.status == McpServerStatus::AwaitingTrust)
            .map(|info| info.name.clone())
            .collect();
        if !awaiting.is_empty() {
            warn!(
                servers = %awaiting.join(", "),
                "MCP trust changed after preflight; leaving servers parked"
            );
        }
    }
    handle
}

pub(crate) async fn preflight_mcp(
    cwd: &Path,
    servers: &[McpServer],
    remote: bool,
) -> Result<(), AcpError> {
    if remote {
        if !servers.is_empty() {
            return Err(invalid_params(REMOTE_SESSION_SERVERS));
        }
        return Ok(());
    }
    let (awaiting, errors) = mcp::pending_startup_trust(cwd, injected_servers(servers)).await;
    if !errors.is_empty() {
        warn!(%errors, "MCP config errors");
    }
    if awaiting.is_empty() {
        return Ok(());
    }
    let message = format!(
        "project MCP servers require startup trust: {}. Run `caudra`, review them with `/mcp`, then retry",
        awaiting.join(", ")
    );
    Err(invalid_params(&message))
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::LoadSessionRequest;
    use caudra_storage::id::CaudraId;

    use super::*;
    use crate::errors::parse_params;

    #[test]
    fn converts_injected_mcp_servers() {
        let raw = serde_json::json!({
            "params": {
                "sessionId": CaudraId::generate().to_string(),
                "cwd": "/project",
                "mcpServers": [
                    {
                        "type": "http",
                        "name": "kan.dev/mcp",
                        "url": "http://127.0.0.1:41012",
                        "headers": [{ "name": "Authorization", "value": "Bearer abc" }]
                    },
                    {
                        "name": "local",
                        "command": "/usr/bin/mcp",
                        "args": ["--stdio"],
                        "env": [{ "name": "TOKEN", "value": "t" }]
                    },
                    {
                        "type": "sse",
                        "name": "legacy",
                        "url": "http://127.0.0.1:41013",
                        "headers": []
                    }
                ]
            }
        });

        let req: LoadSessionRequest = parse_params(&raw).unwrap();
        let servers = injected_servers(&req.mcp_servers);
        assert_eq!(servers.len(), 2, "sse is dropped, not converted");

        let (name, RawTransport::Http(http)) = &servers[0] else {
            panic!("expected http transport");
        };
        assert_eq!(name, "kan-dev-mcp", "wire names are coerced to valid ones");
        assert_eq!(http.url, "http://127.0.0.1:41012");
        assert_eq!(
            http.headers.get("Authorization").map(String::as_str),
            Some("Bearer abc")
        );

        let (name, RawTransport::Stdio(stdio)) = &servers[1] else {
            panic!("expected stdio transport");
        };
        assert_eq!(name, "local");
        assert_eq!(stdio.command, ["/usr/bin/mcp", "--stdio"]);
        assert_eq!(
            stdio.environment.get("TOKEN").map(String::as_str),
            Some("t")
        );
    }
}
