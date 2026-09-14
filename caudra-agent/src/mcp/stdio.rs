use std::collections::HashMap;
#[cfg(unix)]
use std::fs::Permissions;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_lock::Mutex;

use super::config::LocalExecutionPolicy;
use futures_lite::io::BufReader;
use futures_lite::{AsyncBufReadExt, AsyncWriteExt};
use serde_json::Value;
use smol::channel;
use tempfile::{Builder, TempDir};
use tracing::{debug, info, warn};

use super::error::McpError;
use super::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use super::transport::{BoxFuture, McpTransport};
use crate::ChildGuard;

type PendingMap = HashMap<u64, channel::Sender<Result<Value, McpError>>>;

const LINE_DELIMITER: u8 = b'\n';
const REMOTE_TRUST_REQUIRED: &str = "client-local MCP processes require explicit trust; launch isolation is not a filesystem sandbox";
const LOCAL_CWD_PREFIX: &str = "caudra-mcp-";
const LOCAL_TEMP_ROOT: &str = "/tmp";
#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

pub struct StdioTransport {
    name: Arc<str>,
    stdin: Mutex<async_process::ChildStdin>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: AtomicU64,
    timeout: Duration,
    alive: Arc<AtomicBool>,
    _reader_task: smol::Task<()>,
    _stderr_task: smol::Task<()>,
    _child: ChildGuard,
    _local_cwd: Option<TempDir>,
}

impl StdioTransport {
    pub fn spawn(
        name: &str,
        program: &str,
        args: &[String],
        environment: &HashMap<String, String>,
        timeout: Duration,
        policy: LocalExecutionPolicy,
        trusted: bool,
    ) -> Result<Self, McpError> {
        if policy == LocalExecutionPolicy::RemoteLocal && !trusted {
            return Err(McpError::StartFailed {
                server: name.into(),
                reason: REMOTE_TRUST_REQUIRED.into(),
            });
        }
        let mut std_cmd = Command::new(program);
        let local_cwd =
            configure_launch(&mut std_cmd, policy).map_err(|error| McpError::StartFailed {
                server: name.into(),
                reason: error.to_string(),
            })?;
        std_cmd.args(args).envs(environment);

        #[cfg(unix)]
        unsafe {
            std_cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        let mut cmd: async_process::Command = std_cmd.into();
        cmd.stdin(async_process::Stdio::piped())
            .stdout(async_process::Stdio::piped())
            .stderr(async_process::Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| McpError::StartFailed {
            server: name.into(),
            reason: e.to_string(),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stdin".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stdout".into(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stderr".into(),
        })?;

        let name: Arc<str> = Arc::from(name);
        let alive = Arc::new(AtomicBool::new(true));
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

        let reader_task = {
            let name = Arc::clone(&name);
            let alive = Arc::clone(&alive);
            let pending = Arc::clone(&pending);
            smol::spawn(async move {
                let result = Self::reader_loop(&name, &mut BufReader::new(stdout), &pending).await;
                if let Err(e) = &result {
                    warn!(server = &*name, error = %e, "MCP reader loop ended");
                }
                alive.store(false, Ordering::Release);
                for (_, sender) in pending.lock().await.drain() {
                    let _ = sender
                        .send(Err(McpError::ServerDied {
                            server: (*name).into(),
                        }))
                        .await;
                }
            })
        };

        let stderr_task = {
            let name = Arc::clone(&name);
            smol::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                warn!(server = &*name, "{trimmed}");
                            }
                        }
                    }
                }
            })
        };

        Ok(Self {
            name,
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            timeout,
            alive,
            _reader_task: reader_task,
            _stderr_task: stderr_task,
            _child: ChildGuard::new(child),
            _local_cwd: local_cwd,
        })
    }

    async fn reader_loop(
        name: &Arc<str>,
        reader: &mut (impl AsyncBufReadExt + Unpin),
        pending: &Mutex<PendingMap>,
    ) -> Result<(), McpError> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| McpError::ServerDied {
                    server: format!("{}: read failed: {e}", &**name),
                })?;

            if n == 0 {
                return Err(McpError::ServerDied {
                    server: (**name).into(),
                });
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match serde_json::from_str::<JsonRpcResponse>(trimmed) {
                Ok(resp) => {
                    if let Some(id) = resp.id {
                        if let Some(sender) = pending.lock().await.remove(&id) {
                            let result = if let Some(err) = resp.error {
                                Err(McpError::RpcError {
                                    server: (**name).into(),
                                    code: err.code,
                                    message: err.message,
                                })
                            } else {
                                Ok(resp.result.unwrap_or(Value::Null))
                            };
                            let _ = sender.send(result).await;
                        } else {
                            debug!(server = &**name, id, "response for unknown request id");
                        }
                    } else {
                        debug!(server = &**name, "received notification (no id)");
                    }
                }
                Err(e) => {
                    debug!(server = &**name, error = %e, line = trimmed, "non-JSON-RPC line from server");
                }
            }
        }
    }

    fn server(&self) -> String {
        (*self.name).into()
    }

    async fn write_line(&self, line: &[u8]) -> Result<(), McpError> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line)
            .await
            .map_err(|e| McpError::WriteFailed {
                server: self.server(),
                reason: e.to_string(),
            })?;
        stdin.flush().await.map_err(|e| McpError::WriteFailed {
            server: self.server(),
            reason: e.to_string(),
        })
    }

    fn server_died(&self) -> McpError {
        McpError::ServerDied {
            server: self.server(),
        }
    }

    fn serialize(&self, value: &impl serde::Serialize) -> Result<Vec<u8>, McpError> {
        let mut buf = serde_json::to_vec(value).map_err(|e| McpError::InvalidResponse {
            server: self.server(),
            reason: e.to_string(),
        })?;
        buf.push(LINE_DELIMITER);
        Ok(buf)
    }
}

impl McpTransport for StdioTransport {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(async move {
            if !self.alive.load(Ordering::Acquire) {
                return Err(self.server_died());
            }

            let start = Instant::now();
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let req = JsonRpcRequest::new(id, method, params);

            let (tx, rx) = smol::channel::bounded(1);
            self.pending.lock().await.insert(id, tx);

            if let Err(e) = self.write_line(&self.serialize(&req)?).await {
                self.pending.lock().await.remove(&id);
                return Err(e);
            }

            let result = futures_lite::future::race(
                async { rx.recv().await.unwrap_or(Err(self.server_died())) },
                async {
                    async_io::Timer::after(self.timeout).await;
                    Err(McpError::Timeout {
                        server: self.server(),
                        timeout_ms: self.timeout.as_millis() as u64,
                    })
                },
            )
            .await;

            if result.is_err() {
                self.pending.lock().await.remove(&id);
            } else {
                info!(server = %self.server(), method, id, duration_ms = start.elapsed().as_millis() as u64, "MCP stdio response");
            }

            result
        })
    }

    fn send_notification<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            let notif = JsonRpcNotification::new(method, params);
            self.write_line(&self.serialize(&notif)?).await
        })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // Flip `alive` so any in-flight reader or writer gives up with a clean error.
            // We deliberately do not signal the child here: the transport lives behind an
            // Arc, and once the last clone goes away `ChildGuard::drop` takes care of
            // killing the whole process group. Doing it twice just raced with itself.
            self.alive.store(false, Ordering::Release);
        })
    }

    fn server_name(&self) -> &Arc<str> {
        &self.name
    }

    fn transport_kind(&self) -> &'static str {
        "stdio"
    }

    fn child_pids(&self) -> Vec<u32> {
        vec![self._child.id()]
    }
}

fn configure_launch(
    command: &mut Command,
    policy: LocalExecutionPolicy,
) -> io::Result<Option<TempDir>> {
    if policy == LocalExecutionPolicy::Embedded {
        return Ok(None);
    }
    // Fixed host temp root: even TMPDIR must not redirect launch into the workspace.
    let mut builder = Builder::new();
    builder.prefix(LOCAL_CWD_PREFIX);
    #[cfg(unix)]
    builder.permissions(Permissions::from_mode(PRIVATE_DIRECTORY_MODE));
    let cwd = builder.tempdir_in(LOCAL_TEMP_ROOT)?;
    command.env_clear().current_dir(cwd.path());
    Ok(Some(cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::io::Cursor;
    use test_case::test_case;

    const CANARY_ENV: &str = "CAUDRA_MCP_ENV_CANARY";
    const CANARY_VALUE: &str = "launch-secret";
    const CONFIGURED_ENV: &str = "CAUDRA_MCP_CONFIGURED";
    const CONFIGURED_VALUE: &str = "explicit-value";
    const CWD_CANARY: &str = "launch-workspace-canary";
    const SPAWN_CANARY_TEST: &str = "mcp::stdio::tests::spawn_has_no_ambient_launch_context";
    const PROBE_SCRIPT: &str = r#"read -r request
if test -e launch-workspace-canary; then workspace=true; else workspace=false; fi
printf '{"jsonrpc":"2.0","id":1,"result":{"canary":"%s","configured":"%s","cwd":"%s","workspace":%s}}\n' "$CAUDRA_MCP_ENV_CANARY" "$CAUDRA_MCP_CONFIGURED" "$PWD" "$workspace"
cat >/dev/null
"#;

    #[test]
    fn spawn_has_no_ambient_launch_context() {
        if std::env::var(CANARY_ENV).as_deref() != Ok(CANARY_VALUE) {
            let workspace = tempfile::tempdir().unwrap();
            std::fs::write(workspace.path().join(CWD_CANARY), CANARY_VALUE).unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", SPAWN_CANARY_TEST, "--nocapture"])
                .env(CANARY_ENV, CANARY_VALUE)
                .current_dir(workspace.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        smol::block_on(async {
            for policy in [
                LocalExecutionPolicy::Embedded,
                LocalExecutionPolicy::RemoteLocal,
            ] {
                let transport = StdioTransport::spawn(
                    "launch-probe",
                    "/bin/sh",
                    &["-c".into(), PROBE_SCRIPT.into()],
                    &HashMap::from([(CONFIGURED_ENV.into(), CONFIGURED_VALUE.into())]),
                    Duration::from_secs(5),
                    policy,
                    true,
                )
                .unwrap();
                let response = transport.send_request("probe", None).await.unwrap();
                let embedded = policy == LocalExecutionPolicy::Embedded;
                assert_eq!(response["configured"], CONFIGURED_VALUE);
                assert_eq!(response["canary"], if embedded { CANARY_VALUE } else { "" });
                assert_eq!(response["workspace"], embedded);
                let cwd = std::env::current_dir().unwrap();
                assert_eq!(response["cwd"] == cwd.to_str().unwrap(), embedded);
                transport.shutdown().await;
            }
        });
    }

    #[test_case(LocalExecutionPolicy::Embedded; "embedded_inherits")]
    #[test_case(LocalExecutionPolicy::RemoteLocal; "remote_clears")]
    fn launch_context_canaries(policy: LocalExecutionPolicy) {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join(CWD_CANARY), CANARY_VALUE).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .env(CANARY_ENV, CANARY_VALUE)
            .current_dir(workspace.path());
        let cwd = configure_launch(&mut command, policy).unwrap();
        command.env(CONFIGURED_ENV, CONFIGURED_VALUE).args([
            "-c",
            "printf '%s\\n%s\\n' \"$CAUDRA_MCP_ENV_CANARY\" \"$CAUDRA_MCP_CONFIGURED\"; pwd; test ! -e launch-workspace-canary",
        ]);
        let output = command.output().unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[1], CONFIGURED_VALUE);
        if let Some(cwd) = cwd {
            assert_eq!(lines[0], "");
            assert_eq!(lines[2], cwd.path().to_str().unwrap());
            assert!(output.status.success());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    cwd.path().metadata().unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        } else {
            assert_eq!(lines[0], CANARY_VALUE);
            assert_eq!(lines[2], workspace.path().to_str().unwrap());
            assert!(!output.status.success());
        }
    }

    #[test_case(false; "untrusted_refused")]
    #[test_case(true; "trusted_spawned")]
    fn remote_spawn_requires_explicit_trust(trusted: bool) {
        smol::block_on(async {
            let result = StdioTransport::spawn(
                "remote-local",
                "/bin/cat",
                &[],
                &HashMap::new(),
                Duration::from_secs(1),
                LocalExecutionPolicy::RemoteLocal,
                trusted,
            );
            if trusted {
                result.unwrap().shutdown().await;
            } else {
                assert!(
                    matches!(result, Err(McpError::StartFailed { reason, .. }) if reason == REMOTE_TRUST_REQUIRED)
                );
            }
        });
    }

    async fn read_single_response(input: &str) -> Result<Value, McpError> {
        let pending: Mutex<PendingMap> = Mutex::new(HashMap::new());
        let name: Arc<str> = Arc::from("test");

        let (tx, rx) = channel::bounded(1);
        pending.lock().await.insert(1, tx);

        let mut reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let _ = StdioTransport::reader_loop(&name, &mut reader, &pending).await;

        rx.try_recv().unwrap_or(Err(McpError::ServerDied {
            server: "no response received".into(),
        }))
    }

    #[test_case("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n" ; "lf_terminated")]
    #[test_case("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n" ; "crlf_terminated")]
    #[test_case("  {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}  \n" ; "whitespace_padded")]
    #[test_case("\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n" ; "blank_lines_before")]
    #[test_case("not json\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n" ; "invalid_json_before")]
    fn reader_parses_valid_response(input: &str) {
        smol::block_on(async {
            assert!(read_single_response(input).await.is_ok());
        });
    }

    #[test]
    fn reader_returns_rpc_error() {
        let input =
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32600,\"message\":\"bad\"}}\n";
        smol::block_on(async {
            assert!(matches!(
                read_single_response(input).await,
                Err(McpError::RpcError { code: -32600, .. })
            ));
        });
    }
}
