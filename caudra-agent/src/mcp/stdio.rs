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
use std::sync::Mutex as SyncMutex;
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

use super::call::{CallScope, CancelOnDrop, lock, with_progress_token};
use super::error::McpError;
use super::inbound::Inbound;
use super::listen;
use super::message::{self, Message, RequestId, Response};
use super::protocol::{
    JsonRpcNotification, JsonRpcRequest, LISTEN_METHOD, ServerCapabilities,
};
use super::transport::{BoxFuture, McpTransport};
use crate::ChildGuard;

type PendingMap = HashMap<u64, channel::Sender<Result<Value, McpError>>>;
type Stdin = Arc<Mutex<async_process::ChildStdin>>;

const LINE_DELIMITER: u8 = b'\n';
const REMOTE_TRUST_REQUIRED: &str = "client-local MCP processes require explicit trust; launch isolation is not a filesystem sandbox";
const LOCAL_CWD_PREFIX: &str = "caudra-mcp-";
const LOCAL_TEMP_ROOT: &str = "/tmp";
#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

pub struct StdioTransport {
    core: Arc<Core>,
    /// A modern server's subscription, open once it declared a list that can
    /// change. Dropping the task cancels it.
    listener: SyncMutex<Option<smol::Task<()>>>,
    _reader_task: smol::Task<()>,
    _writer_task: smol::Task<()>,
    _stderr_task: smol::Task<()>,
    _child: ChildGuard,
    _local_cwd: Option<TempDir>,
}

/// What sending a request takes, shared with the subscription's task.
struct Core {
    name: Arc<str>,
    stdin: Stdin,
    /// Lines written without a caller waiting on the write: replies to the
    /// server's requests, and cancellations sent from a drop.
    outbox: flume::Sender<Vec<u8>>,
    pending: Arc<Mutex<PendingMap>>,
    inbound: Arc<Inbound>,
    next_id: AtomicU64,
    timeout: Duration,
    alive: Arc<AtomicBool>,
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
        let inbound = Arc::new(Inbound::new(Arc::clone(&name)));
        let stdin: Stdin = Arc::new(Mutex::new(stdin));
        let (outbox, outbox_rx) = flume::unbounded::<Vec<u8>>();

        let writer_task = {
            let name = Arc::clone(&name);
            let stdin = Arc::clone(&stdin);
            smol::spawn(async move {
                while let Ok(line) = outbox_rx.recv_async().await {
                    if let Err(error) = write_to(&stdin, &line).await {
                        debug!(server = &*name, %error, "MCP stdio writer stopped");
                        return;
                    }
                }
            })
        };

        let reader_task = {
            let name = Arc::clone(&name);
            let alive = Arc::clone(&alive);
            let pending = Arc::clone(&pending);
            let inbound = Arc::clone(&inbound);
            let outbox = outbox.clone();
            smol::spawn(async move {
                let result = Self::reader_loop(
                    &name,
                    &mut BufReader::new(stdout),
                    &pending,
                    &inbound,
                    &outbox,
                )
                .await;
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
            core: Arc::new(Core {
                name,
                stdin,
                outbox,
                pending,
                inbound,
                next_id: AtomicU64::new(1),
                timeout,
                alive,
            }),
            listener: SyncMutex::default(),
            _reader_task: reader_task,
            _writer_task: writer_task,
            _stderr_task: stderr_task,
            _child: ChildGuard::new(child),
            _local_cwd: local_cwd,
        })
    }

    /// A response settles the call waiting on its id. A request from the
    /// server is answered on its own task, against the newest call that can
    /// ask someone, since stdio ties it to no call.
    async fn reader_loop(
        name: &Arc<str>,
        reader: &mut (impl AsyncBufReadExt + Unpin),
        pending: &Mutex<PendingMap>,
        inbound: &Arc<Inbound>,
        outbox: &flume::Sender<Vec<u8>>,
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

            match message::parse(trimmed) {
                Some(Message::Response(response)) => resolve(name, pending, response).await,
                Some(Message::Request(request)) => {
                    let route = inbound.routed();
                    let inbound = Arc::clone(inbound);
                    let outbox = outbox.clone();
                    smol::spawn(async move {
                        if let Some(reply) = inbound.answer(request, route).await {
                            let _ = outbox.send(line_of(&reply));
                        }
                    })
                    .detach();
                }
                Some(Message::Notification(notification)) => inbound.notify(notification),
                None => {
                    debug!(
                        server = &**name,
                        line = trimmed,
                        "non-JSON-RPC line from server"
                    );
                }
            }
        }
    }
}

impl Core {
    fn server(&self) -> String {
        (*self.name).into()
    }

    async fn write_line(&self, line: &[u8]) -> Result<(), McpError> {
        write_to(&self.stdin, line)
            .await
            .map_err(|e| McpError::WriteFailed {
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

    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        scope: &CallScope,
    ) -> Result<Value, McpError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(self.server_died());
        }

        let start = Instant::now();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let params = self.inbound.era().with_meta(params, scope);
        let req = JsonRpcRequest::new(id, method, with_progress_token(method, params, id));

        let (tx, rx) = smol::channel::bounded(1);
        self.pending.lock().await.insert(id, tx);
        let _tracked = self.inbound.track(id, scope);

        if let Err(e) = self.write_line(&self.serialize(&req)?).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        let mut cancel = CancelOnDrop::new(method, id, |notification| {
            let _ = self.outbox.send(line_of(&notification));
        });
        let result = scope
            .wait(&self.name, self.timeout, async {
                rx.recv().await.unwrap_or(Err(self.server_died()))
            })
            .await;
        cancel.settle(&result);
        self.pending.lock().await.remove(&id);
        if result.is_ok() {
            info!(server = %self.server(), method, id, duration_ms = start.elapsed().as_millis() as u64, "MCP stdio response");
        }
        result
    }
}

impl McpTransport for StdioTransport {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        scope: &'a CallScope,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(self.core.request(method, params, scope))
    }

    fn send_notification<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            let notif = JsonRpcNotification::new(method, params);
            self.core.write_line(&self.core.serialize(&notif)?).await
        })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            lock(&self.listener).take();
            // Flip `alive` so any in-flight reader or writer gives up with a clean error.
            // We deliberately do not signal the child here: the transport lives behind an
            // Arc, and once the last clone goes away `ChildGuard::drop` takes care of
            // killing the whole process group. Doing it twice just raced with itself.
            self.core.alive.store(false, Ordering::Release);
        })
    }

    fn server_name(&self) -> &Arc<str> {
        &self.core.name
    }

    fn transport_kind(&self) -> &'static str {
        "stdio"
    }

    fn child_pids(&self) -> Vec<u32> {
        vec![self._child.id()]
    }

    fn inbound(&self) -> Option<&Inbound> {
        Some(&self.core.inbound)
    }

    fn listen(&self, capabilities: &ServerCapabilities) {
        let mut listener = lock(&self.listener);
        if listener.is_some() || self.core.inbound.era().modern().is_none() {
            return;
        }
        let core = Arc::clone(&self.core);
        *listener = Some(smol::spawn(listen::subscribe(
            Arc::clone(&core.name),
            capabilities.listen_filter(),
            move |params| {
                let core = Arc::clone(&core);
                Box::pin(async move {
                    let scope = CallScope::standing();
                    core.request(LISTEN_METHOD, Some(params), &scope).await
                })
            },
        )));
    }
}

async fn write_to(stdin: &Stdin, line: &[u8]) -> io::Result<()> {
    let mut stdin = stdin.lock().await;
    stdin.write_all(line).await?;
    stdin.flush().await
}

fn line_of(message: &Value) -> Vec<u8> {
    let mut line = message.to_string().into_bytes();
    line.push(LINE_DELIMITER);
    line
}

async fn resolve(name: &str, pending: &Mutex<PendingMap>, response: Response) {
    let Some(id) = response.id.as_ref().and_then(RequestId::as_u64) else {
        debug!(server = name, "MCP response names no request of ours");
        return;
    };
    match pending.lock().await.remove(&id) {
        Some(sender) => {
            let _ = sender.send(response.into_result(name)).await;
        }
        None => debug!(server = name, id, "response for unknown request id"),
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
    use serde_json::json;
    use test_case::test_case;

    use crate::cancel::CancelToken;
    use crate::mcp::McpToolMeta;
    use crate::mcp::call::{ElicitMode, ElicitRequest, ElicitResponse, Elicitor};
    use crate::mcp::elicitation::ElicitAction;
    use crate::mcp::protocol::{CANCELLED_NOTIFICATION, METHOD_NOT_FOUND, TOOLS_CALL_METHOD};
    use crate::mcp::transport;
    use crate::types::Answerer;

    const SERVER_REQUEST_ENV: &str = "SERVER_REQUEST";
    const SERVER_REQUEST_ID: &str = "srv-1";
    const PROBE_METHOD: &str = "probe";
    const ELICIT_MESSAGE: &str = "Which branch?";
    const FIELD: &str = "branch";
    const ANSWER: &str = "main";
    /// Sends `$SERVER_REQUEST` while the first call is open, then answers
    /// the call with whatever Caudra replied to it.
    const ECHO_REPLY_SCRIPT: &str = r#"read -r call
printf '%s\n' "$SERVER_REQUEST"
read -r reply
printf '{"jsonrpc":"2.0","id":1,"result":{"reply":%s}}\n' "$reply"
cat >/dev/null
"#;
    /// Never answers the first call; answers the second with the line that
    /// cancelled the first, whichever order the two arrived in.
    const ECHO_CANCEL_SCRIPT: &str = r#"read -r call
read -r first
read -r second
case "$first" in *notifications/cancelled*) cancelled="$first" ;; *) cancelled="$second" ;; esac
printf '{"jsonrpc":"2.0","id":2,"result":{"cancelled":%s}}\n' "$cancelled"
cat >/dev/null
"#;
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
                let response = transport
                    .send_request("probe", None, &CallScope::none())
                    .await
                    .unwrap();
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
        let inbound = Arc::new(Inbound::new(Arc::clone(&name)));

        let (tx, rx) = channel::bounded(1);
        pending.lock().await.insert(1, tx);

        let mut reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let _ = StdioTransport::reader_loop(
            &name,
            &mut reader,
            &pending,
            &inbound,
            &flume::unbounded().0,
        )
        .await;

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

    fn scripted(script: &str, environment: HashMap<String, String>) -> StdioTransport {
        StdioTransport::spawn(
            "scripted",
            "/bin/sh",
            &["-c".into(), script.into()],
            &environment,
            Duration::from_secs(5),
            LocalExecutionPolicy::Embedded,
            true,
        )
        .unwrap()
    }

    /// What Caudra replied to `request`, sent by the server mid-call.
    fn reply_to(request: Value, scope: &CallScope) -> Value {
        let transport = scripted(
            ECHO_REPLY_SCRIPT,
            HashMap::from([(SERVER_REQUEST_ENV.into(), request.to_string())]),
        );
        smol::block_on(async {
            let result = transport
                .send_request(TOOLS_CALL_METHOD, None, scope)
                .await
                .unwrap();
            transport.shutdown().await;
            result["reply"].clone()
        })
    }

    fn elicitation() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": SERVER_REQUEST_ID,
            "method": "elicitation/create",
            "params": {
                "message": ELICIT_MESSAGE,
                "requestedSchema": {"type": "object", "properties": {FIELD: {"type": "string"}}},
            },
        })
    }

    #[test]
    fn a_server_request_reusing_a_pending_id_is_answered_not_taken_as_the_response() {
        let reply = reply_to(
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}),
            &CallScope::none(),
        );
        assert_eq!(reply, json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
    }

    #[test_case("roots/list" ; "roots")]
    #[test_case("sampling/createMessage" ; "sampling")]
    #[test_case("vendor/unknown" ; "unknown")]
    fn unsupported_server_requests_are_refused(method: &str) {
        let reply = reply_to(
            json!({"jsonrpc": "2.0", "id": SERVER_REQUEST_ID, "method": method}),
            &CallScope::none(),
        );
        assert_eq!(reply["id"], SERVER_REQUEST_ID);
        assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn an_elicitation_nobody_can_answer_is_declined() {
        let reply = reply_to(elicitation(), &CallScope::none());
        assert_eq!(reply["result"], json!({"action": "decline"}));
    }

    struct Accepting;

    impl Elicitor for Accepting {
        fn elicit<'a>(
            &'a self,
            _server: &'a str,
            request: ElicitRequest,
        ) -> BoxFuture<'a, ElicitResponse> {
            Box::pin(async move {
                assert!(matches!(request.mode, ElicitMode::Form { .. }));
                ElicitResponse {
                    action: ElicitAction::Accept,
                    content: json!({FIELD: ANSWER}).as_object().cloned(),
                    by: Answerer::User,
                    answers: vec![(FIELD.into(), ANSWER.into())],
                }
            })
        }
    }

    #[test]
    fn an_elicitation_reaches_the_calls_elicitor_and_is_recorded() {
        let scope = CallScope::new(Some(Arc::new(Accepting)), None, CancelToken::none());
        let reply = reply_to(elicitation(), &scope);
        assert_eq!(
            reply["result"],
            json!({"action": "accept", "content": {FIELD: ANSWER}})
        );
        let interactions = scope.take_interactions();
        assert_eq!(interactions.len(), 1);
        assert_eq!(interactions[0].message, ELICIT_MESSAGE);
        assert_eq!(interactions[0].action, ElicitAction::Accept);
        assert_eq!(
            interactions[0].answers,
            [(FIELD.to_owned(), ANSWER.to_owned())]
        );
    }

    const STATE_ENV: &str = "STATE";
    /// As written on the wire: raw UTF-8, escaped quotes and backslashes,
    /// and a run that looks like the header sentinel.
    const WIRE_STATE: &str = r#"état \"opaque\" \\ =?base64?x?="#;
    const TOOL: &str = "t";
    /// Asks for nothing but another try with `$STATE`, then answers the
    /// retry with both calls and whether the state came back byte for byte.
    const STATE_ONLY_SCRIPT: &str = r#"id_of() { printf '%s' "$1" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p'; }
read -r first
printf '{"jsonrpc":"2.0","id":%s,"result":{"resultType":"input_required","requestState":"%s"}}\n' "$(id_of "$first")" "$STATE"
read -r second
case "$second" in *"\"requestState\":\"$STATE\""*) exact=true ;; *) exact=false ;; esac
printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[],"structuredContent":{"first":%s,"second":%s,"exact":%s}}}\n' "$(id_of "$second")" "$first" "$second" "$exact"
cat >/dev/null
"#;

    #[test]
    fn a_retried_round_is_a_new_request_echoing_the_state_byte_for_byte() {
        let transport = scripted(
            STATE_ONLY_SCRIPT,
            HashMap::from([(STATE_ENV.into(), WIRE_STATE.into())]),
        );
        let output = smol::block_on(transport::call_tool(
            &transport,
            TOOL,
            &McpToolMeta::default(),
            None,
            &json!({}),
            &CallScope::none(),
        ))
        .unwrap();
        let echoed = output.structured_content().unwrap();
        assert_eq!(echoed["exact"], true, "{echoed}");
        assert_ne!(echoed["first"]["id"], echoed["second"]["id"]);
        let retry = &echoed["second"]["params"];
        assert_eq!(retry["name"], TOOL);
        assert!(retry.get("inputResponses").is_none(), "{retry}");
        smol::block_on(transport.shutdown());
    }

    #[test]
    fn a_cancelled_call_tells_the_server() {
        let transport = scripted(ECHO_CANCEL_SCRIPT, HashMap::new());
        let (trigger, cancel) = CancelToken::new();
        let scope = CallScope::new(None, None, cancel);
        trigger.cancel();
        smol::block_on(async {
            let call = transport
                .send_request(TOOLS_CALL_METHOD, None, &scope)
                .await;
            assert!(matches!(call, Err(McpError::Cancelled { .. })), "{call:?}");
            let probe = transport
                .send_request(PROBE_METHOD, None, &CallScope::none())
                .await
                .unwrap();
            assert_eq!(probe["cancelled"]["method"], CANCELLED_NOTIFICATION);
            assert_eq!(probe["cancelled"]["params"]["requestId"], 1);
            transport.shutdown().await;
        });
    }
}
