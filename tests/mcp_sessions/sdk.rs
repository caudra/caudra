//! Caudra's binary in SDK mode, driven over stream-json the way an SDK client drives it, in a home
//! of its own: fresh config, state and data directories, with server-everything configured.
//!
//! The model's answers come from a cassette per process under `cassettes/<test>/`. With
//! `CAUDRA_IT_RECORD=<provider/model>` the test asks that model instead, and once it passes, its
//! answers replace the cassettes. A login is copied from this machine's debug build, only while it
//! has long enough left that the copy never refreshes it: a refreshed copy would sign the
//! original out.

use std::cell::Cell;
use std::env;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use caudra_storage::paths;
use serde_json::{Value, json};
use tempfile::TempDir;

const BINARY: &str = env!("CARGO_BIN_EXE_caudra");
const CASSETTES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mcp_sessions/cassettes");
const RECORD_ENV: &str = "CAUDRA_IT_RECORD";
const PROVIDER_RECORD_ENV: &str = "CAUDRA_PROVIDER_RECORD";
const PROVIDER_REPLAY_ENV: &str = "CAUDRA_PROVIDER_REPLAY";
const NAMESPACE_ENV: &str = "CAUDRA_NAMESPACE";
const NAMESPACE: &str = "caudra-mcp-it";
const XDG_HOMES: [(&str, &str); 4] = [
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_STATE_HOME", "state"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_CACHE_HOME", "cache"),
];
const CONFIG_HOME: &str = "config";
const STATE_HOME: &str = "state";
const WORK_DIR: &str = "work";
const RECORDINGS_DIR: &str = "recordings";
const AUTH_DIR: &str = "auth";
const CONFIG_FILE: &str = "caudra.toml";
const MCP_FILE: &str = "mcp.toml";
const CONFIG: &str = "[agent]\ngenerate_titles = false\n";
const MCP_CONFIG: &str = "[mcp.everything]\ncommand = [\"docker\", \"run\", \"--interactive\", \"--rm\", \"caudra-mcp-it/everything\"]\n";
const PRIVATE_DIR: u32 = 0o700;
const PRIVATE_FILE: u32 = 0o600;
const EXPIRES_FIELD: &str = "expires";
const MODEL_FIELD: &str = "model";
const LOGIN_MARGIN: Duration = Duration::from_secs(30 * 60);
const TURN_TIMEOUT: Duration = Duration::from_secs(300);
const DENIED: &str = "declined by the test";
const STALE_LOGIN: &str = "the saved login has under 30 minutes left, and a copy that refreshed it would sign this machine out: log in again with a debug build, then record";
const NO_CASSETTE: &str =
    "no cassette: record it with CAUDRA_IT_RECORD=openai/gpt-6-luna make test-mcp-servers";
const NO_RESULT: &str = "caudra finished no turn in time";
const UNANSWERED: &str = "caudra asked more often than the test answers";

/// How the test answers one permission prompt.
#[derive(Clone, Copy, Debug)]
pub enum Reply {
    Allow,
    /// Allows the tool for the rest of the session, as an SDK permission update.
    AllowForSession,
    Deny,
}

impl Reply {
    fn response(self, tool: &str) -> Value {
        match self {
            Self::Allow => json!({"behavior": "allow"}),
            Self::AllowForSession => json!({
                "behavior": "allow",
                "updatedPermissions": [{
                    "type": "addRules",
                    "behavior": "allow",
                    "destination": "session",
                    "rules": [{"toolName": tool}],
                }],
            }),
            Self::Deny => json!({"behavior": "deny", "message": DENIED}),
        }
    }
}

/// What one turn did: the tools it asked about, the tool results, and how it ended.
#[derive(Debug, Default)]
pub struct Turn {
    pub asked: Vec<String>,
    pub results: Vec<String>,
    pub outcome: String,
}

impl Turn {
    pub fn mentions(&self, text: &str) -> bool {
        self.results.iter().any(|result| result.contains(text))
    }
}

pub struct Home {
    root: TempDir,
    cassettes: PathBuf,
    recording: Option<String>,
    runs: Cell<usize>,
}

impl Home {
    pub fn new(test: &str) -> Self {
        let root = TempDir::new().expect("tempdir");
        let config = root.path().join(CONFIG_HOME).join(NAMESPACE);
        private_dir(&config);
        fs::write(config.join(CONFIG_FILE), CONFIG).expect("config");
        fs::write(config.join(MCP_FILE), MCP_CONFIG).expect("MCP config");
        for dir in [WORK_DIR, RECORDINGS_DIR] {
            private_dir(&root.path().join(dir));
        }
        let recording = env::var(RECORD_ENV).ok();
        if let Some(model) = &recording {
            copy_login(model, &root.path().join(STATE_HOME).join(NAMESPACE));
        }
        Self {
            root,
            cassettes: Path::new(CASSETTES).join(test),
            recording,
            runs: Cell::new(0),
        }
    }

    /// Starts `caudra -p` in `permission_mode`, resuming the session `resume` names, if any.
    pub fn start(&self, permission_mode: &str, resume: Option<&str>) -> Process {
        let run = self.runs.replace(self.runs.get() + 1);
        let cassette = format!("{run}.json");
        let mut command = Command::new(BINARY);
        let model = match &self.recording {
            Some(model) => {
                let path = self.root.path().join(RECORDINGS_DIR).join(&cassette);
                command
                    .env(PROVIDER_RECORD_ENV, path)
                    .env_remove(PROVIDER_REPLAY_ENV);
                model.clone()
            }
            None => {
                let path = self.cassettes.join(&cassette);
                let model = recorded_model(&path);
                command
                    .env(PROVIDER_REPLAY_ENV, path)
                    .env_remove(PROVIDER_RECORD_ENV);
                model
            }
        };
        command.args([
            "-p",
            "-m",
            &model,
            "--permission-mode",
            permission_mode,
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ]);
        if let Some(session) = resume {
            command.args(["-s", session]);
        }
        for (variable, dir) in XDG_HOMES {
            command.env(variable, self.root.path().join(dir));
        }
        let mut child = command
            .env(NAMESPACE_ENV, NAMESPACE)
            .current_dir(self.root.path().join(WORK_DIR))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("caudra starts");
        let stdout = child.stdout.take().expect("piped stdout");
        let (lines, events) = flume::unbounded();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(event) = serde_json::from_str::<Value>(&line)
                    && lines.send(event).is_err()
                {
                    break;
                }
            }
        });
        Process {
            stdin: child.stdin.take(),
            child,
            events,
            session: None,
        }
    }
}

impl Drop for Home {
    /// A recording that passed replaces the test's cassettes.
    fn drop(&mut self) {
        if self.recording.is_none() || thread::panicking() {
            return;
        }
        match fs::remove_dir_all(&self.cassettes) {
            Err(error) if error.kind() != ErrorKind::NotFound => panic!("{error}"),
            _ => {}
        }
        fs::create_dir_all(&self.cassettes).expect("cassette directory");
        let recordings = fs::read_dir(self.root.path().join(RECORDINGS_DIR)).expect("recordings");
        for recording in recordings {
            let recording = recording.expect("recording").path();
            let name = recording.file_name().expect("recording name");
            fs::copy(&recording, self.cassettes.join(name)).expect("cassette");
        }
    }
}

pub struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    events: flume::Receiver<Value>,
    session: Option<String>,
}

impl Process {
    /// Sends `prompt` and answers each permission prompt with the next of `replies`, until the
    /// turn ends.
    pub fn turn(&mut self, prompt: &str, replies: &[Reply]) -> Turn {
        self.send(&json!({"type": "user", "message": {"role": "user", "content": prompt}}));
        let mut replies = replies.iter();
        let mut turn = Turn::default();
        loop {
            let event = self.events.recv_timeout(TURN_TIMEOUT).expect(NO_RESULT);
            match event["type"].as_str() {
                Some("control_request") if event["request"]["subtype"] == "can_use_tool" => {
                    let tool = event["request"]["tool_name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let reply = replies.next().expect(UNANSWERED);
                    self.send(&json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "success",
                            "request_id": event["request_id"],
                            "response": reply.response(&tool),
                        },
                    }));
                    turn.asked.push(tool);
                }
                Some("user") => turn.results.extend(tool_results(&event)),
                Some("result") => {
                    self.session = event["session_id"].as_str().map(str::to_owned);
                    turn.outcome = event["subtype"].as_str().unwrap_or_default().to_owned();
                    return turn;
                }
                _ => {}
            }
        }
    }

    /// Ends the session the way an SDK client does, by closing stdin, and returns its id.
    pub fn finish(mut self) -> String {
        drop(self.stdin.take());
        let status = self.child.wait().expect("caudra exits");
        assert!(status.success(), "caudra exited with {status}");
        self.session.take().expect("a turn named the session")
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").expect("caudra reads stdin");
        stdin.flush().expect("caudra reads stdin");
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn tool_results(event: &Value) -> Vec<String> {
    let blocks = event["message"]["content"].as_array().into_iter().flatten();
    blocks
        .filter(|block| block["type"] == "tool_result")
        .map(|block| match &block["content"] {
            Value::String(text) => text.clone(),
            parts => parts
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        })
        .collect()
}

fn recorded_model(cassette: &Path) -> String {
    let text = fs::read_to_string(cassette)
        .unwrap_or_else(|error| panic!("{NO_CASSETTE} ({}: {error})", cassette.display()));
    let cassette: Value = serde_json::from_str(&text).expect("cassette JSON");
    cassette[MODEL_FIELD]
        .as_str()
        .expect("the cassette names its model")
        .to_owned()
}

fn private_dir(path: &Path) {
    DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIR)
        .create(path)
        .expect("private directory");
}

/// Copies this machine's login for `model`'s provider into `state`. Without one, an API key in
/// the environment logs in instead.
fn copy_login(model: &str, state: &Path) {
    let provider = model
        .split_once('/')
        .map_or(model, |(provider, _)| provider);
    let login = format!("{provider}.json");
    let source = paths::state_dir_path()
        .expect("state directory")
        .join(AUTH_DIR)
        .join(&login);
    let Ok(text) = fs::read_to_string(&source) else {
        return;
    };
    let saved: Value = serde_json::from_str(&text).expect("login JSON");
    if let Some(expires) = saved[EXPIRES_FIELD].as_u64() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970");
        let left = Duration::from_millis(expires).saturating_sub(now);
        assert!(left > LOGIN_MARGIN, "{STALE_LOGIN}: {}", source.display());
    }
    let auth = state.join(AUTH_DIR);
    private_dir(&auth);
    let mut copy = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE)
        .open(auth.join(login))
        .expect("login copy");
    copy.write_all(text.as_bytes()).expect("login copy");
}
