//! The real server loop over in-memory pipes, with a scripted model behind
//! every session. Each test runs in a child copy of this binary whose state,
//! data, config, and cache directories are fresh, because a session's store
//! resolves from the process's own directories.

use std::collections::HashMap;
use std::env;
use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, BufRead, BufReader, PipeWriter, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use caudra_agent::AgentConfig;
use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::tools::ToolRegistry;
use caudra_agent::tools::test_support::{GUARDED_TOOL_NAME, GuardedMock, NamedMock};
use caudra_config::ModelPolicy;
use caudra_providers::model::Model;
use caudra_providers::provider::{BoxFuture, Provider};
use caudra_providers::{
    AgentError, CacheKey, ContentBlock, Message, ModelInfo, ProviderEvent, RequestOptions, Role,
    StopReason, StreamResponse, Timeouts, TokenUsage,
};
use caudra_storage::StateDir;
use color_eyre::Result as ServeResult;
use flume::{Receiver, RecvTimeoutError, Sender};
use serde_json::{Value, json};
use smol::Unblock;
use tempfile::TempDir;

use crate::server::{Discovery, serve_on};
use crate::test_support::{SELECTED_SPEC, TestAccounts};
use crate::{AcpParams, AcpRuntime, AcpRuntimeResolver, locked};

const CHILD_ENV: &str = "CAUDRA_ACP_E2E_CHILD";
const NAMESPACE_ENV: &str = "CAUDRA_NAMESPACE";
const ISOLATED_DIRS: [&str; 4] = [
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
];
/// Lets the real provider resolve before the script replaces it; nothing is sent with it.
const API_KEY_ENV: &str = "OPENAI_API_KEY";
const FAKE_API_KEY: &str = "sk-e2e-never-sent";
const ONE_PASSED: &str = "1 passed";
/// Caudra writes plans only below directories no one else can write to.
const PRIVATE_DIR_MODE: u32 = 0o700;
/// How long a frame may take before the test fails instead of hanging.
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const INPUT_TOKENS: u32 = 120;
const OUTPUT_TOKENS: u32 = 8;
const PROTOCOL_VERSION: u64 = 1;
const EMPTY_INPUT: &str = "{}";

const SESSION_UPDATE: &str = "session/update";
const REQUEST_PERMISSION: &str = "session/request_permission";
const PERMISSION_OUTCOME_SELECTED: &str = "selected";

/// Runs `scenario` in a child copy of this test binary, so nothing it stores
/// reaches the machine's own directories.
fn isolated(scenario: impl FnOnce()) {
    if env::var_os(CHILD_ENV).is_some() {
        return scenario();
    }
    let name = thread::current()
        .name()
        .expect("libtest names each test's thread")
        .to_owned();
    let home = tempfile::Builder::new()
        .permissions(Permissions::from_mode(PRIVATE_DIR_MODE))
        .tempdir()
        .unwrap();
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args(["--exact", &name, "--nocapture"])
        .env(CHILD_ENV, "1")
        .env(API_KEY_ENV, FAKE_API_KEY)
        .env_remove(NAMESPACE_ENV);
    for dir in ISOLATED_DIRS {
        let path = home.path().join(dir);
        DirBuilder::new()
            .mode(PRIVATE_DIR_MODE)
            .create(&path)
            .unwrap();
        command.env(dir, path);
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains(ONE_PASSED),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// One model turn as the scripted provider plays it.
enum Reply {
    Text(&'static str),
    Tool {
        id: &'static str,
    },
    /// Never answers, as a model still thinking when the turn is cancelled.
    Hang,
}

/// Plays the replies a test queued, one per request, waiting for the next
/// one when none is queued.
struct ScriptedModel {
    replies: Receiver<Reply>,
    requests: Sender<()>,
}

impl Provider for ScriptedModel {
    fn stream_message<'a>(
        &'a self,
        _: &'a Model,
        _: &'a [Message],
        _: &'a str,
        _: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&'a CacheKey>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let _ = self.requests.send(());
            let reply = self.replies.recv_async().await.expect("the script ended");
            let (content, stop_reason) = match reply {
                Reply::Text(text) => {
                    let _ = event_tx.send(ProviderEvent::TextDelta { text: text.into() });
                    (
                        ContentBlock::Text { text: text.into() },
                        StopReason::EndTurn,
                    )
                }
                Reply::Tool { id } => {
                    let _ = event_tx.send(ProviderEvent::ToolUseStart {
                        id: id.into(),
                        name: GUARDED_TOOL_NAME.into(),
                        source_ordinal: None,
                    });
                    let _ = event_tx.send(ProviderEvent::ToolInputDelta {
                        id: id.into(),
                        delta: EMPTY_INPUT.into(),
                    });
                    (
                        ContentBlock::ToolUse {
                            id: id.into(),
                            name: GUARDED_TOOL_NAME.into(),
                            input: json!({}),
                            thought_signature: None,
                        },
                        StopReason::ToolUse,
                    )
                }
                Reply::Hang => smol::future::pending().await,
            };
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![content],
                    ..Default::default()
                },
                usage: TokenUsage {
                    input: INPUT_TOKENS,
                    output: OUTPUT_TOKENS,
                    ..Default::default()
                },
                stop_reason: Some(stop_reason),
                ..Default::default()
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// The model behind every session opened in one project directory.
struct Script {
    replies: Sender<Reply>,
    requests: Receiver<()>,
}

impl Script {
    /// Queues `reply`. The requests made before it are forgotten, so
    /// [`Self::requested`] waits for one that comes after.
    fn reply(&self, reply: Reply) {
        self.requests.drain();
        self.replies.send(reply).unwrap();
    }

    /// Waits until the agent asks the model for its next turn.
    fn requested(&self) {
        self.requests
            .recv_timeout(FRAME_TIMEOUT)
            .expect("the agent asks the model");
    }
}

type Models = Arc<Mutex<HashMap<PathBuf, Arc<ScriptedModel>>>>;

fn runtime_for(models: &Models, cwd: &Path) -> Result<AcpRuntime, String> {
    let model = locked(models)
        .get(cwd)
        .cloned()
        .ok_or_else(|| format!("no script for {}", cwd.display()))?;
    let registry = ToolRegistry::new();
    registry
        .register(Arc::new(GuardedMock), NamedMock::source())
        .map_err(|error| error.to_string())?;
    Ok(AcpRuntime {
        config: AgentConfig {
            generate_titles: false,
            summarize_memory: false,
            ..AgentConfig::default()
        },
        registry: Arc::new(registry),
        provider: Some(model),
        ..AcpRuntime::default()
    })
}

/// A JSON-RPC client on the other end of the server's pipes.
struct Client {
    input: Option<PipeWriter>,
    frames: Receiver<Value>,
    next_id: i64,
    models: Models,
    served: Receiver<ServeResult<()>>,
    projects: TempDir,
}

impl Client {
    fn start() -> Self {
        Self::opening_on(SELECTED_SPEC)
    }

    /// A server whose new sessions open on `spec`.
    fn opening_on(spec: &str) -> Self {
        let (server_in, input) = io::pipe().unwrap();
        let (output, server_out) = io::pipe().unwrap();
        let models = Models::default();
        let resolver_models = Arc::clone(&models);
        let resolver: AcpRuntimeResolver =
            Arc::new(move |cwd, _| runtime_for(&resolver_models, &cwd));
        let params = AcpParams {
            model: Model::from_spec(spec).unwrap(),
            timeouts: Timeouts::default(),
            thinking: Default::default(),
            prompt_profiles: Arc::new(PromptProfileCatalog::default()),
            system_prompt_profile_override: None,
            permission_mode: None,
            model_policy: Arc::new(ModelPolicy::default()),
            runtime_resolver: resolver,
            accounts: TestAccounts::new(true),
        };
        let storage = StateDir::resolve().unwrap();
        let (served_tx, served) = flume::bounded(1);
        thread::spawn(move || {
            let served = smol::block_on(serve_on(
                params,
                storage,
                smol::io::BufReader::new(Unblock::new(server_in)),
                Unblock::new(server_out),
                Discovery::Off,
            ));
            let _ = served_tx.send(served);
        });
        let (frame_tx, frames) = flume::unbounded();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                let frame = serde_json::from_str(&line).expect("every frame is JSON");
                if frame_tx.send(frame).is_err() {
                    break;
                }
            }
        });
        Self {
            input: Some(input),
            frames,
            next_id: 0,
            models,
            served,
            projects: TempDir::new().unwrap(),
        }
    }

    /// A project directory whose sessions answer from the returned script.
    fn project(&self, name: &str) -> (PathBuf, Script) {
        let cwd = self.projects.path().join(name);
        fs::create_dir_all(&cwd).unwrap();
        let script = self.script_for(&cwd);
        (cwd, script)
    }

    /// The script sessions in `cwd` answer from on this server.
    fn script_for(&self, cwd: &Path) -> Script {
        let (replies_tx, replies) = flume::unbounded();
        let (requests_tx, requests) = flume::unbounded();
        let model = ScriptedModel {
            replies,
            requests: requests_tx,
        };
        locked(&self.models).insert(cwd.to_path_buf(), Arc::new(model));
        Script {
            replies: replies_tx,
            requests,
        }
    }

    fn send_raw(&mut self, line: &[u8]) {
        let input = self.input.as_mut().expect("input is open");
        input.write_all(line).unwrap();
        input.write_all(b"\n").unwrap();
    }

    fn send(&mut self, message: &Value) {
        self.send_raw(&serde_json::to_vec(message).unwrap());
    }

    fn request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn next(&self) -> Value {
        match self.frames.recv_timeout(FRAME_TIMEOUT) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => panic!("no frame within {FRAME_TIMEOUT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("the server closed its output"),
        }
    }

    /// Every frame up to and including the first that `last` accepts.
    fn until(&self, last: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut frames = Vec::new();
        loop {
            let frame = self.next();
            let done = last(&frame);
            frames.push(frame);
            if done {
                return frames;
            }
        }
    }

    /// Every frame up to and including the response to `id`.
    fn until_response(&self, id: i64) -> Vec<Value> {
        self.until(|frame| is_response(frame, id))
    }

    fn response(&self, id: i64) -> Value {
        self.until_response(id).pop().unwrap()
    }

    /// The responses to `ids`, in that order whatever order they arrive in.
    fn responses(&self, ids: &[i64]) -> Vec<Value> {
        let mut answered = HashMap::new();
        while answered.len() < ids.len() {
            let frame = self.next();
            if let Some(&id) = ids.iter().find(|&&id| is_response(&frame, id)) {
                answered.insert(id, frame);
            }
        }
        ids.iter().map(|id| answered.remove(id).unwrap()).collect()
    }

    /// Every frame up to the response to `id`, answering each permission
    /// request with the option of kind `answer`.
    fn turn(&mut self, id: i64, answer: &str) -> Vec<Value> {
        let mut frames = Vec::new();
        loop {
            let frame = self.next();
            if frame["method"] == REQUEST_PERMISSION {
                let option = option_of_kind(&frame, answer);
                let outcome = json!({"outcome": {"outcome": PERMISSION_OUTCOME_SELECTED, "optionId": option}});
                self.send(&json!({"jsonrpc": "2.0", "id": frame["id"], "result": outcome}));
            }
            let done = is_response(&frame, id);
            frames.push(frame);
            if done {
                return frames;
            }
        }
    }

    fn initialize(&mut self) -> Value {
        let id = self.request(methods::INITIALIZE, orca_initialize());
        self.response(id)["result"].take()
    }

    /// Opens a session in `cwd` and returns its id and the response, after
    /// checking its commands are announced right behind the response.
    fn open(&mut self, cwd: &Path) -> (Value, Value) {
        let id = self.request(methods::SESSION_NEW, json!({"cwd": cwd, "mcpServers": []}));
        let frames = self.until_response(id);
        assert_eq!(frames.len(), 1, "nothing precedes the response: {frames:?}");
        let mut result = frames[0]["result"].clone();
        assert_eq!(
            update_kind(&self.next()),
            Some(updates::AVAILABLE_COMMANDS),
            "commands follow the response"
        );
        (result["sessionId"].take(), result)
    }

    fn prompt(&mut self, sid: &Value, text: &str) -> i64 {
        self.request(methods::SESSION_PROMPT, prompt_params(sid, text))
    }

    fn cancel(&mut self, sid: &Value) {
        self.notify(methods::SESSION_CANCEL, json!({"sessionId": sid}));
    }

    /// Closes the server's input and waits for the loop to end, returning
    /// what it ended with and every frame it wrote on the way out.
    fn hang_up(&mut self) -> (ServeResult<()>, Vec<Value>) {
        drop(self.input.take());
        let served = self
            .served
            .recv_timeout(FRAME_TIMEOUT)
            .expect("the server ends once its input does");
        let mut frames = Vec::new();
        while let Ok(frame) = self.frames.recv_timeout(FRAME_TIMEOUT) {
            frames.push(frame);
        }
        (served, frames)
    }
}

mod methods {
    pub(super) const INITIALIZE: &str = "initialize";
    pub(super) const SESSION_NEW: &str = "session/new";
    pub(super) const SESSION_PROMPT: &str = "session/prompt";
    pub(super) const SESSION_CANCEL: &str = "session/cancel";
    pub(super) const SESSION_LOAD: &str = "session/load";
    pub(super) const SESSION_LIST: &str = "session/list";
    pub(super) const SESSION_FORK: &str = "session/fork";
    pub(super) const SESSION_CLOSE: &str = "session/close";
    pub(super) const SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
}

mod updates {
    pub(super) const AGENT_MESSAGE_CHUNK: &str = "agent_message_chunk";
    pub(super) const USER_MESSAGE_CHUNK: &str = "user_message_chunk";
    pub(super) const USAGE: &str = "usage_update";
    pub(super) const TOOL_CALL: &str = "tool_call";
    pub(super) const TOOL_CALL_UPDATE: &str = "tool_call_update";
    pub(super) const AVAILABLE_COMMANDS: &str = "available_commands_update";
    pub(super) const CONFIG_OPTION: &str = "config_option_update";
    pub(super) const CURRENT_MODE: &str = "current_mode_update";
}

fn is_response(frame: &Value, id: i64) -> bool {
    frame["id"] == id && frame.get("method").is_none()
}

fn update_kind(frame: &Value) -> Option<&str> {
    if frame["method"] != SESSION_UPDATE {
        return None;
    }
    frame["params"]["update"]["sessionUpdate"].as_str()
}

fn option_of_kind<'a>(request: &'a Value, kind: &str) -> &'a Value {
    let options = request["params"]["options"].as_array().unwrap();
    &options
        .iter()
        .find(|option| option["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} option in {request}"))["optionId"]
}

fn prompt_params(sid: &Value, text: &str) -> Value {
    json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]})
}

/// What Orca sends: no file system, no terminal, nothing optional.
fn orca_initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false,
        },
        "clientInfo": {"name": "orca", "version": "1"},
    })
}

mod tests {
    use std::path::Path;

    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{
        Client, INPUT_TOKENS, OUTPUT_TOKENS, PROTOCOL_VERSION, REQUEST_PERMISSION, Reply, Script,
        is_response, isolated, methods, option_of_kind, prompt_params, update_kind, updates,
    };
    use crate::outbound::CANCEL_REQUEST_METHOD;
    use crate::test_support::{CANCELLED, OFFLINE_SPEC, SELECTED_SPEC};
    use crate::transport::MAX_LINE_BYTES;

    const PROJECT: &str = "project";
    const OTHER_PROJECT: &str = "other";
    const PROMPT: &str = "rename foo to bar";
    const REPLY: &str = "Renamed.";
    const OTHER_REPLY: &str = "Nothing to rename.";
    const CLIENT_PROMPT_ID: &str = "client-prompt-1";
    const TOOL_ID: &str = "toolu_guarded";
    const END_TURN: &str = "end_turn";
    const COMPLETED: &str = "completed";
    const FAILED: &str = "failed";
    const ALLOW_ONCE: &str = "allow_once";
    const RESOURCE_NOT_FOUND: i64 = -32002;
    const INVALID_REQUEST: i64 = -32600;
    const UNKNOWN_SESSION: &str = "no-such-session";
    const LOGIN_METHOD: &str = "caudra-login";
    const SESSION_CAPABILITIES: [&str; 5] = ["list", "delete", "fork", "resume", "close"];
    const MODEL_CATEGORY: &str = "model";
    const THOUGHT_LEVEL_CATEGORY: &str = "thought_level";
    const EFFORT_LEVEL: &str = "low";
    /// Out of the range the client numbers its own requests from.
    const OVERSIZED_ID: i64 = 9_000;
    const GARBAGE: &[u8] = b"{ this is not json";

    fn kinds(frames: &[Value]) -> Vec<&str> {
        frames.iter().filter_map(update_kind).collect()
    }

    fn position(frames: &[Value], found: impl Fn(&Value) -> bool) -> usize {
        frames
            .iter()
            .position(found)
            .unwrap_or_else(|| panic!("frame missing from {frames:#?}"))
    }

    fn option<'a>(options: &'a Value, field: &str, value: &str) -> &'a Value {
        options
            .as_array()
            .unwrap()
            .iter()
            .find(|option| option[field] == value)
            .unwrap_or_else(|| panic!("no option with {field} {value} in {options}"))
    }

    fn set_option(client: &mut Client, sid: &Value, config: &Value, value: &str) -> Vec<Value> {
        let id = client.request(
            methods::SESSION_SET_CONFIG_OPTION,
            json!({"sessionId": sid, "configId": config, "value": value}),
        );
        client.until_response(id)
    }

    /// A session that answered one prompt, so it is stored.
    fn prompted(client: &mut Client, cwd: &Path, script: &Script) -> Value {
        let (sid, _) = client.open(cwd);
        script.reply(Reply::Text(REPLY));
        let id = client.prompt(&sid, PROMPT);
        assert_eq!(client.response(id)["result"]["stopReason"], END_TURN);
        sid
    }

    #[test]
    fn initialize_advertises_what_clients_build_on() {
        isolated(|| {
            let mut client = Client::start();
            let result = client.initialize();
            assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
            let capabilities = &result["agentCapabilities"];
            assert_eq!(capabilities["loadSession"], true);
            for capability in SESSION_CAPABILITIES {
                assert!(
                    capabilities["sessionCapabilities"][capability].is_object(),
                    "{capability} missing from {capabilities}"
                );
            }
            assert_eq!(capabilities["promptCapabilities"]["embeddedContext"], true);
            assert_eq!(capabilities["_meta"]["caudra"]["steering"], true);
            assert_eq!(result["authMethods"][0]["id"], LOGIN_METHOD);
            assert_eq!(result["_meta"]["caudra"]["defaultModel"], SELECTED_SPEC);
        });
    }

    #[test_case(Some(CLIENT_PROMPT_ID) ; "client_prompt_id")]
    #[test_case(None ; "generated_prompt_id")]
    fn a_text_reply_streams_its_chunk_and_usage_before_end_turn(prompt_id: Option<&str>) {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, script) = client.project(PROJECT);
            let (sid, _) = client.open(&cwd);
            script.reply(Reply::Text(REPLY));
            let mut params = prompt_params(&sid, PROMPT);
            if let Some(prompt_id) = prompt_id {
                params["_meta"] = json!({"promptId": prompt_id});
            }
            let id = client.request(methods::SESSION_PROMPT, params);

            let frames = client.until_response(id);

            assert_eq!(
                kinds(&frames),
                [updates::AGENT_MESSAGE_CHUNK, updates::USAGE]
            );
            assert_eq!(frames.len(), 3, "{frames:#?}");
            let chunk = &frames[0]["params"];
            assert_eq!(chunk["sessionId"], sid);
            assert_eq!(chunk["update"]["content"]["text"], REPLY);
            assert!(chunk["update"]["messageId"].is_string(), "{chunk}");
            let turn_id = &chunk["_meta"]["caudra"]["promptId"];
            if let Some(prompt_id) = prompt_id {
                assert_eq!(turn_id, prompt_id);
            }
            assert_eq!(frames[1]["params"]["_meta"]["caudra"]["promptId"], *turn_id);
            let result = &frames[2]["result"];
            assert_eq!(result["stopReason"], END_TURN);
            assert_eq!(result["_meta"]["caudra"]["promptId"], *turn_id);
            assert_eq!(result["usage"]["inputTokens"], INPUT_TOKENS);
            assert_eq!(result["usage"]["outputTokens"], OUTPUT_TOKENS);
        });
    }

    #[test]
    fn a_tool_call_waits_for_permission_then_completes() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, script) = client.project(PROJECT);
            let (sid, _) = client.open(&cwd);
            script.reply(Reply::Tool { id: TOOL_ID });
            script.reply(Reply::Text(REPLY));
            let id = client.prompt(&sid, PROMPT);

            let frames = client.turn(id, ALLOW_ONCE);

            let announced = position(&frames, |frame| {
                update_kind(frame) == Some(updates::TOOL_CALL)
                    && frame["params"]["update"]["toolCallId"] == TOOL_ID
            });
            let asked = position(&frames, |frame| frame["method"] == REQUEST_PERMISSION);
            assert_eq!(frames[asked]["params"]["toolCall"]["toolCallId"], TOOL_ID);
            assert_eq!(frames[asked]["params"]["sessionId"], sid);
            let completed = position(&frames, |frame| {
                update_kind(frame) == Some(updates::TOOL_CALL_UPDATE)
                    && frame["params"]["update"]["status"] == COMPLETED
            });
            let replied = position(&frames, |frame| {
                update_kind(frame) == Some(updates::AGENT_MESSAGE_CHUNK)
            });
            // The ask carries the call's own title and input, so it can come
            // before the call is announced, which waits for the go-ahead.
            assert!(
                asked < announced && announced < completed && completed < replied,
                "{frames:#?}"
            );
            assert_eq!(frames.last().unwrap()["result"]["stopReason"], END_TURN);
        });
    }

    #[test]
    fn a_cancel_mid_turn_withdraws_the_ask_and_fails_the_open_tool() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, script) = client.project(PROJECT);
            let (sid, _) = client.open(&cwd);
            script.reply(Reply::Tool { id: TOOL_ID });
            let id = client.prompt(&sid, PROMPT);
            let asked = client
                .until(|frame| frame["method"] == REQUEST_PERMISSION)
                .pop()
                .unwrap();

            client.cancel(&sid);
            let frames = client.until_response(id);

            position(&frames, |frame| {
                frame["method"] == CANCEL_REQUEST_METHOD
                    && frame["params"]["requestId"] == asked["id"]
            });
            position(&frames, |frame| {
                update_kind(frame) == Some(updates::TOOL_CALL_UPDATE)
                    && frame["params"]["update"]["toolCallId"] == TOOL_ID
                    && frame["params"]["update"]["status"] == FAILED
            });
            assert_eq!(frames.last().unwrap()["result"]["stopReason"], CANCELLED);

            script.reply(Reply::Text(REPLY));
            let next = client.prompt(&sid, PROMPT);
            assert_eq!(client.response(next)["result"]["stopReason"], END_TURN);
        });
    }

    #[test]
    fn concurrent_sessions_get_only_their_own_updates() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, script) = client.project(PROJECT);
            let (other_cwd, other_script) = client.project(OTHER_PROJECT);
            let (sid, _) = client.open(&cwd);
            let (other_sid, _) = client.open(&other_cwd);
            let first = client.prompt(&sid, PROMPT);
            let second = client.prompt(&other_sid, PROMPT);
            script.requested();
            other_script.requested();

            other_script.reply(Reply::Text(OTHER_REPLY));
            let other_frames = client.until_response(second);
            script.reply(Reply::Text(REPLY));
            let frames = client.until_response(first);

            for (frames, sid, reply) in [
                (&other_frames, &other_sid, OTHER_REPLY),
                (&frames, &sid, REPLY),
            ] {
                let updates: Vec<&Value> = frames
                    .iter()
                    .filter(|frame| update_kind(frame).is_some())
                    .collect();
                assert!(!updates.is_empty());
                for update in updates {
                    assert_eq!(update["params"]["sessionId"], *sid, "{update}");
                }
                let chunk = position(frames, |frame| {
                    update_kind(frame) == Some(updates::AGENT_MESSAGE_CHUNK)
                });
                assert_eq!(frames[chunk]["params"]["update"]["content"]["text"], reply);
                assert_eq!(frames.last().unwrap()["result"]["stopReason"], END_TURN);
            }
        });
    }

    #[test]
    fn a_prompt_to_an_unknown_session_is_not_found() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let id = client.prompt(&json!(UNKNOWN_SESSION), PROMPT);
            assert_eq!(client.response(id)["error"]["code"], RESOURCE_NOT_FOUND);
        });
    }

    #[test]
    fn a_second_prompt_to_a_busy_session_is_refused() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, script) = client.project(PROJECT);
            let (sid, _) = client.open(&cwd);
            script.reply(Reply::Hang);
            let running = client.prompt(&sid, PROMPT);
            script.requested();

            let refused = client.prompt(&sid, PROMPT);
            assert_eq!(client.response(refused)["error"]["code"], INVALID_REQUEST);

            client.cancel(&sid);
            assert_eq!(client.response(running)["result"]["stopReason"], CANCELLED);
        });
    }

    /// The session is stored by one server and loaded by the next, which
    /// opens new sessions on another model.
    #[test]
    fn a_load_replays_the_conversation_and_keeps_the_stored_model() {
        isolated(|| {
            let mut first = Client::opening_on(OFFLINE_SPEC);
            first.initialize();
            let (cwd, script) = first.project(PROJECT);
            let sid = prompted(&mut first, &cwd, &script);
            first.hang_up().0.unwrap();
            let mut client = Client::start();
            client.initialize();
            client.script_for(&cwd);

            let load = client.request(
                methods::SESSION_LOAD,
                json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}),
            );
            let frames = client.until_response(load);

            let replayed: Vec<(&str, &Value)> = frames
                .iter()
                .filter_map(|frame| {
                    let kind = update_kind(frame)?;
                    [updates::USER_MESSAGE_CHUNK, updates::AGENT_MESSAGE_CHUNK]
                        .contains(&kind)
                        .then(|| (kind, &frame["params"]["update"]["content"]["text"]))
                })
                .collect();
            assert_eq!(
                replayed,
                [
                    (updates::USER_MESSAGE_CHUNK, &json!(PROMPT)),
                    (updates::AGENT_MESSAGE_CHUNK, &json!(REPLY)),
                ]
            );
            let result = &frames.last().unwrap()["result"];
            let model = option(&result["configOptions"], "category", MODEL_CATEGORY);
            assert_eq!(model["currentValue"], OFFLINE_SPEC);
        });
    }

    #[test]
    fn a_stored_session_is_listed_with_its_directory() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, script) = client.project(PROJECT);
            let sid = prompted(&mut client, &cwd, &script);

            let id = client.request(methods::SESSION_LIST, json!({}));
            let result = client.response(id)["result"].take();

            let listed = option(&result["sessions"], "sessionId", sid.as_str().unwrap());
            assert_eq!(listed["cwd"], json!(cwd));
        });
    }

    #[test]
    fn a_fork_is_a_new_session_that_takes_prompts() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, script) = client.project(PROJECT);
            let sid = prompted(&mut client, &cwd, &script);

            let id = client.request(
                methods::SESSION_FORK,
                json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}),
            );
            let forked = client.response(id)["result"]["sessionId"].take();

            assert!(forked.is_string() && forked != sid, "{forked}");
            script.reply(Reply::Text(REPLY));
            let id = client.prompt(&forked, PROMPT);
            let frames = client.until_response(id);
            assert_eq!(frames.last().unwrap()["result"]["stopReason"], END_TURN);
            for frame in frames.iter().filter(|frame| update_kind(frame).is_some()) {
                assert_eq!(frame["params"]["sessionId"], forked);
            }
        });
    }

    #[test]
    fn a_closed_session_is_not_found() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, _script) = client.project(PROJECT);
            let (sid, _) = client.open(&cwd);
            let close = client.request(methods::SESSION_CLOSE, json!({"sessionId": sid}));
            assert!(client.response(close)["result"].is_object());

            let id = client.prompt(&sid, PROMPT);
            assert_eq!(client.response(id)["error"]["code"], RESOURCE_NOT_FOUND);
        });
    }

    #[test_case("effort", "high", &[updates::CONFIG_OPTION] ; "effort")]
    #[test_case("mode", "plan", &[updates::CURRENT_MODE, updates::CONFIG_OPTION] ; "mode")]
    fn setting_an_option_answers_the_full_list_and_announces_it(
        config: &str,
        value: &str,
        announced: &[&str],
    ) {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, _script) = client.project(PROJECT);
            let (sid, opened) = client.open(&cwd);
            let offered: Vec<&Value> = opened["configOptions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|option| &option["id"])
                .collect();

            let mut frames = set_option(&mut client, &sid, &json!(config), value);
            // Both updates are queued before the response, which a client reads last.
            let result = frames.pop().unwrap()["result"].take();

            let answered: Vec<&Value> = result["configOptions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|option| &option["id"])
                .collect();
            assert_eq!(answered, offered);
            assert_eq!(
                option(&result["configOptions"], "id", config)["currentValue"],
                value
            );
            assert_eq!(kinds(&frames), announced, "{frames:#?}");
            let update = position(&frames, |frame| {
                update_kind(frame) == Some(updates::CONFIG_OPTION)
            });
            let update = &frames[update]["params"]["update"]["configOptions"];
            assert_eq!(option(update, "id", config)["currentValue"], value);
        });
    }

    #[test]
    fn garbage_and_oversized_lines_leave_the_server_serving() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            client.send_raw(GARBAGE);
            let oversized = json!({
                "jsonrpc": "2.0",
                "id": OVERSIZED_ID,
                "method": methods::SESSION_LIST,
                "params": {"pad": "x".repeat(MAX_LINE_BYTES)},
            });
            client.send(&oversized);
            let refused = client.response(OVERSIZED_ID);
            assert_eq!(refused["error"]["code"], INVALID_REQUEST);

            let id = client.request(methods::SESSION_LIST, json!({}));
            assert!(client.response(id)["result"]["sessions"].is_array());
        });
    }

    #[test]
    fn end_of_input_answers_the_running_prompt_and_ends_serving() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, script) = client.project(PROJECT);
            let (sid, _) = client.open(&cwd);
            script.reply(Reply::Hang);
            let id = client.prompt(&sid, PROMPT);
            script.requested();

            let (served, frames) = client.hang_up();

            served.unwrap();
            let answered = &frames[position(&frames, |frame| is_response(frame, id))];
            assert_eq!(answered["result"]["stopReason"], CANCELLED);
        });
    }

    #[test]
    fn a_second_cancel_answers_the_prompt_waiting_behind_the_first() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, script) = client.project(PROJECT);
            let (sid, _) = client.open(&cwd);
            script.reply(Reply::Hang);
            let first = client.prompt(&sid, PROMPT);
            script.requested();

            client.cancel(&sid);
            let waiting = client.prompt(&sid, PROMPT);
            client.cancel(&sid);

            for answered in client.responses(&[first, waiting]) {
                assert_eq!(answered["result"]["stopReason"], CANCELLED, "{answered}");
            }
        });
    }

    /// Orca drives Caudra as a generic ACP agent: the exact requests it
    /// sends, its pickers drawn by category, and steering as cancel plus a
    /// new prompt sent right behind it.
    #[test]
    fn orca_generic_client_contract() {
        isolated(|| {
            let mut client = Client::start();
            client.initialize();
            let (cwd, script) = client.project(PROJECT);
            let (sid, opened) = client.open(&cwd);
            let options = &opened["configOptions"];
            let model = option(options, "category", MODEL_CATEGORY)["id"].clone();
            let effort = option(options, "category", THOUGHT_LEVEL_CATEGORY)["id"].clone();

            // Ends on the model it opened on: a turn on another model would
            // build that model's real provider in place of the script.
            for (config, value) in [
                (&model, OFFLINE_SPEC),
                (&effort, EFFORT_LEVEL),
                (&model, SELECTED_SPEC),
            ] {
                let frames = set_option(&mut client, &sid, config, value);
                let result = &frames.last().unwrap()["result"]["configOptions"];
                let id = config.as_str().unwrap();
                assert_eq!(option(result, "id", id)["currentValue"], value);
            }

            script.reply(Reply::Tool { id: TOOL_ID });
            script.reply(Reply::Text(REPLY));
            let id = client.prompt(&sid, PROMPT);
            let frames = client.turn(id, ALLOW_ONCE);
            let asked = position(&frames, |frame| frame["method"] == REQUEST_PERMISSION);
            assert_eq!(option_of_kind(&frames[asked], ALLOW_ONCE), ALLOW_ONCE);
            assert_eq!(frames.last().unwrap()["result"]["stopReason"], END_TURN);

            script.reply(Reply::Hang);
            let steered = client.prompt(&sid, PROMPT);
            script.requested();
            client.cancel(&sid);
            script.reply(Reply::Text(OTHER_REPLY));
            let steering = client.prompt(&sid, PROMPT);

            let frames = client.until_response(steered);
            assert_eq!(frames.last().unwrap()["result"]["stopReason"], CANCELLED);
            let frames = client.until_response(steering);
            let answered = frames.last().unwrap();
            assert_eq!(answered["result"]["stopReason"], END_TURN, "{answered}");
        });
    }
}
